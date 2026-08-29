// GitHub handler: renders a repo, file, tree, commit, issue, pull request,
// issues list, or Actions run/job from the api.github.com REST API.

use super::util::{build_result, format_media_duration, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use async_trait::async_trait;
use base64::Engine as _;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct GitHubHandler;

const README_TREE_LIMIT: usize = 100;
const ISSUES_LIST_LIMIT: usize = 30;
const COMMENTS_PER_PAGE: usize = 100;

static LOG_TIMESTAMP: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)^\d{4}-\d{2}-\d{2}T[\d:.]+Z ").expect("static gh log ts regex"));
static README_MD: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)^readme\.md$").expect("static readme regex"));

/// The GitHub token: the stored credential first, then `GITHUB_TOKEN`/`GH_TOKEN`.
///
/// Rule: any key the app needs must be enterable from the TUI, so a value
/// written into the auth store under the `github` id wins; the env vars stay as
/// a headless fallback. A token is optional here and only raises the rate limit.
pub(super) fn github_token() -> Option<String> {
    let stored = mikmik_core::AuthStore::load().api_key_for("github");
    if let Some(token) = stored.filter(|t| !t.is_empty()) {
        return Some(token);
    }
    for var in ["GITHUB_TOKEN", "GH_TOKEN"] {
        if let Ok(token) = std::env::var(var) {
            if !token.is_empty() {
                return Some(token);
            }
        }
    }
    None
}

fn api_headers(accept: &str) -> Vec<(String, String)> {
    let mut headers = vec![("Accept".to_string(), accept.to_string())];
    if let Some(token) = github_token() {
        headers.push(("Authorization".to_string(), format!("Bearer {token}")));
    }
    headers
}

/// GET `https://api.github.com{endpoint}` as JSON, or `None` on any failure.
pub(super) async fn fetch_github_api(endpoint: &str, timeout: Duration) -> Option<Value> {
    let result = load_page(
        &format!("https://api.github.com{endpoint}"),
        LoadOptions {
            timeout,
            headers: api_headers("application/vnd.github.v3+json"),
            ..Default::default()
        },
    )
    .await;
    result
        .ok
        .then(|| serde_json::from_str(&result.content).ok())?
}

async fn fetch_text(
    url: &str,
    timeout: Duration,
    headers: Vec<(String, String)>,
) -> Option<String> {
    let result = load_page(
        url,
        LoadOptions {
            timeout,
            headers,
            ..Default::default()
        },
    )
    .await;
    (result.ok && !result.content.is_empty()).then_some(result.content)
}

// --- URL parsing ---

enum Target {
    Blob {
        git_ref: String,
        path: String,
    },
    Tree {
        git_ref: Option<String>,
        path: String,
    },
    Commit(String),
    Issue(i64),
    Pull(i64),
    IssuesList,
    Repo,
    ActionsRun(i64),
    ActionsJob(i64),
}

struct Gh {
    owner: String,
    repo: String,
    target: Target,
}

fn digits(s: Option<&str>) -> Option<i64> {
    s.filter(|v| v.chars().all(|c| c.is_ascii_digit()) && !v.is_empty())
        .and_then(|v| v.parse().ok())
}

fn parse_actions(sub: &[&str]) -> Option<Target> {
    if sub.first() != Some(&"runs") {
        return None;
    }
    let run_id = digits(sub.get(1).copied())?;
    let seg = sub.get(2).copied();
    if (seg == Some("job") || seg == Some("jobs")) && sub.get(3).is_some() {
        if let Some(job_id) = digits(sub.get(3).copied()) {
            return Some(Target::ActionsJob(job_id));
        }
    }
    Some(Target::ActionsRun(run_id))
}

fn parse_section(section: &str, sub: &[&str]) -> Option<Target> {
    match section {
        "blob" => Some(Target::Blob {
            git_ref: sub.first().unwrap_or(&"").to_string(),
            path: sub.get(1..).map(|p| p.join("/")).unwrap_or_default(),
        }),
        "tree" => Some(Target::Tree {
            git_ref: sub.first().map(|s| s.to_string()),
            path: sub.get(1..).map(|p| p.join("/")).unwrap_or_default(),
        }),
        "commit" => sub
            .first()
            .filter(|s| !s.is_empty())
            .map(|s| Target::Commit(s.to_string())),
        "issues" => Some(digits(sub.first().copied()).map_or(Target::IssuesList, Target::Issue)),
        "pull" => digits(sub.first().copied()).map(Target::Pull),
        "actions" => parse_actions(sub),
        _ => None,
    }
}

fn parse_github_url(url: &str) -> Option<Gh> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.host_str()? != "github.com" {
        return None;
    }
    let parts: Vec<&str> = parsed.path().split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() < 2 {
        return None;
    }
    let owner = parts[0].to_string();
    let repo = parts[1].to_string();
    let target = if parts.len() == 2 {
        Target::Repo
    } else {
        parse_section(parts[2], &parts[3..])?
    };
    Some(Gh {
        owner,
        repo,
        target,
    })
}

// --- value helpers ---

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn login_of(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|u| str_field(u, "login"))
        .map(str::to_string)
}

fn name_list(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|l| str_field(l, "name").map(str::to_string))
        .collect()
}

fn i64_field(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}

// --- blob / tree / repo ---

fn decode_base64_content(content: &str) -> Option<String> {
    let cleaned: String = content.split_whitespace().collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(cleaned)
        .ok()?;
    String::from_utf8(bytes).ok()
}

async fn render_blob(gh: &Gh, git_ref: &str, path: &str, timeout: Duration) -> Option<String> {
    let raw = format!(
        "https://raw.githubusercontent.com/{}/{}/{git_ref}/{path}",
        gh.owner, gh.repo
    );
    fetch_text(&raw, timeout, Vec::new()).await
}

fn sort_contents(items: &mut [Value]) {
    items.sort_by(|a, b| {
        let a_dir = str_field(a, "type") == Some("dir");
        let b_dir = str_field(b, "type") == Some("dir");
        match (a_dir, b_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => str_field(a, "name").cmp(&str_field(b, "name")),
        }
    });
}

fn append_contents(md: &mut String, items: &[Value]) {
    md.push_str("## Contents\n\n```\n");
    for item in items {
        let is_dir = str_field(item, "type") == Some("dir");
        let prefix = if is_dir { "[dir] " } else { "      " };
        let name = str_field(item, "name").unwrap_or("");
        let size = item.get("size").and_then(Value::as_u64);
        let suffix = match (is_dir, size) {
            (false, Some(bytes)) if bytes > 0 => format!(" ({bytes} bytes)"),
            _ => String::new(),
        };
        let _ = writeln!(md, "{prefix}{name}{suffix}");
    }
    md.push_str("```\n\n");
}

async fn render_tree(
    gh: &Gh,
    git_ref: &Option<String>,
    path: &str,
    timeout: Duration,
) -> Option<String> {
    let repo = fetch_github_api(&format!("/repos/{}/{}", gh.owner, gh.repo), timeout).await?;
    let default_branch = str_field(&repo, "default_branch").unwrap_or("main");
    let git_ref = git_ref
        .clone()
        .unwrap_or_else(|| default_branch.to_string());
    let full = str_field(&repo, "full_name").unwrap_or("");
    let display = if path.is_empty() { "(root)" } else { path };
    let mut md = format!("# {full}/{display}\n\n**Branch:** {git_ref}\n\n");

    let contents = fetch_github_api(
        &format!(
            "/repos/{}/{}/contents/{path}?ref={git_ref}",
            gh.owner, gh.repo
        ),
        timeout,
    )
    .await;
    if let Some(mut items) = contents.and_then(|c| c.as_array().cloned()) {
        sort_contents(&mut items);
        append_contents(&mut md, &items);
        append_tree_readme(&mut md, gh, &git_ref, path, &items, timeout).await;
    }
    Some(md)
}

async fn append_tree_readme(
    md: &mut String,
    gh: &Gh,
    git_ref: &str,
    path: &str,
    items: &[Value],
    timeout: Duration,
) {
    let readme = items.iter().find(|item| {
        str_field(item, "type") == Some("file")
            && str_field(item, "name").is_some_and(|n| README_MD.is_match(n))
    });
    let Some(name) = readme.and_then(|r| str_field(r, "name")) else {
        return;
    };
    let readme_path = if path.is_empty() {
        name.to_string()
    } else {
        format!("{path}/{name}")
    };
    let raw = format!(
        "https://raw.githubusercontent.com/{}/{}/{git_ref}/{readme_path}",
        gh.owner, gh.repo
    );
    if let Some(content) = fetch_text(&raw, timeout, Vec::new()).await {
        let _ = write!(md, "---\n\n## README\n\n{content}");
    }
}

fn append_repo_header(md: &mut String, repo: &Value) {
    let full = str_field(repo, "full_name").unwrap_or("");
    let _ = write!(md, "# {full}\n\n");
    if let Some(description) = str_field(repo, "description") {
        let _ = write!(md, "{description}\n\n");
    }
    let _ = writeln!(
        md,
        "Stars: {} · Forks: {} · Issues: {}",
        i64_field(repo, "stargazers_count"),
        i64_field(repo, "forks_count"),
        i64_field(repo, "open_issues_count")
    );
    if let Some(language) = str_field(repo, "language") {
        let _ = writeln!(md, "Language: {language}");
    }
    if let Some(license) = repo.get("license").and_then(|l| str_field(l, "name")) {
        let _ = writeln!(md, "License: {license}");
    }
    md.push_str("\n---\n\n");
}

fn append_repo_tree(md: &mut String, tree: &Value) {
    let Some(entries) = tree.get("tree").and_then(Value::as_array) else {
        return;
    };
    md.push_str("## Files\n\n```\n");
    for entry in entries.iter().take(README_TREE_LIMIT) {
        let prefix = if str_field(entry, "type") == Some("tree") {
            "[dir] "
        } else {
            "      "
        };
        let _ = writeln!(md, "{prefix}{}", str_field(entry, "path").unwrap_or(""));
    }
    if entries.len() > README_TREE_LIMIT {
        let _ = writeln!(md, "[…{} files elided…]", entries.len() - README_TREE_LIMIT);
    }
    md.push_str("```\n\n");
}

async fn render_repo(gh: &Gh, timeout: Duration) -> Option<String> {
    let repo = fetch_github_api(&format!("/repos/{}/{}", gh.owner, gh.repo), timeout).await?;
    let mut md = String::new();
    append_repo_header(&mut md, &repo);
    let default_branch = str_field(&repo, "default_branch").unwrap_or("main");
    if let Some(tree) = fetch_github_api(
        &format!(
            "/repos/{}/{}/git/trees/{default_branch}?recursive=1",
            gh.owner, gh.repo
        ),
        timeout,
    )
    .await
    {
        append_repo_tree(&mut md, &tree);
    }
    if let Some(readme) =
        fetch_github_api(&format!("/repos/{}/{}/readme", gh.owner, gh.repo), timeout).await
    {
        if str_field(&readme, "encoding") == Some("base64") {
            if let Some(decoded) = str_field(&readme, "content").and_then(decode_base64_content) {
                let _ = write!(md, "## README\n\n{decoded}");
            }
        }
    }
    Some(md)
}

// --- commit ---

async fn render_commit(gh: &Gh, git_ref: &str, timeout: Duration) -> Option<String> {
    let commit = fetch_github_api(
        &format!("/repos/{}/{}/commits/{git_ref}", gh.owner, gh.repo),
        timeout,
    )
    .await?;
    let sha = str_field(&commit, "sha").unwrap_or("");
    let message = commit
        .pointer("/commit/message")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut lines = message.splitn(2, '\n');
    let subject = lines.next().unwrap_or("");
    let body = lines.next().unwrap_or("").trim();

    let author = login_of(&commit, "author")
        .map(|l| format!("@{l}"))
        .or_else(|| {
            commit
                .pointer("/commit/author/name")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string());
    let authored = commit
        .pointer("/commit/author/date")
        .and_then(Value::as_str)
        .unwrap_or("");

    let short_subject = if subject.is_empty() {
        &sha[..sha.len().min(7)]
    } else {
        subject
    };
    let mut md = format!("# {short_subject}\n\n");
    let _ = write!(
        md,
        "**{}** · authored by {author}",
        &sha[..sha.len().min(12)]
    );
    if !authored.is_empty() {
        let _ = write!(md, " · {authored}");
    }
    md.push('\n');
    append_commit_stats(&mut md, &commit);
    if !body.is_empty() {
        let _ = write!(md, "\n{body}\n");
    }
    append_commit_files(&mut md, &commit);
    Some(md)
}

fn append_commit_stats(md: &mut String, commit: &Value) {
    if let Some(stats) = commit.get("stats") {
        let additions = i64_field(stats, "additions");
        let deletions = i64_field(stats, "deletions");
        let count = commit
            .get("files")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let plural = if count == 1 { "" } else { "s" };
        let _ = writeln!(
            md,
            "{count} file{plural} changed · +{additions} −{deletions}"
        );
    }
    if let Some(parents) = commit
        .get("parents")
        .and_then(Value::as_array)
        .filter(|p| !p.is_empty())
    {
        let shas: Vec<String> = parents
            .iter()
            .filter_map(|p| str_field(p, "sha"))
            .map(|s| s[..s.len().min(12)].to_string())
            .collect();
        let _ = writeln!(md, "Parents: {}", shas.join(", "));
    }
}

fn append_commit_files(md: &mut String, commit: &Value) {
    let Some(files) = commit
        .get("files")
        .and_then(Value::as_array)
        .filter(|f| !f.is_empty())
    else {
        return;
    };
    let _ = write!(md, "\n---\n\n## Files ({})\n\n", files.len());
    for file in files {
        let filename = str_field(file, "filename").unwrap_or("");
        let name = match str_field(file, "previous_filename") {
            Some(prev) => format!("{prev} → {filename}"),
            None => filename.to_string(),
        };
        let _ = write!(md, "### {name}\n\n");
        let _ = write!(
            md,
            "{} · +{} −{}\n\n",
            str_field(file, "status").unwrap_or(""),
            i64_field(file, "additions"),
            i64_field(file, "deletions")
        );
        match str_field(file, "patch") {
            Some(patch) => {
                let _ = write!(md, "```diff\n{patch}\n```\n\n");
            }
            None => md.push_str("*No textual diff (binary or too large).*\n\n"),
        }
    }
}

// --- issues / pulls ---

async fn fetch_issue_comments(
    gh: &Gh,
    number: i64,
    expected: i64,
    timeout: Duration,
) -> Vec<Value> {
    let mut comments = Vec::new();
    let mut page = 1;
    while (comments.len() as i64) < expected {
        let endpoint = format!(
            "/repos/{}/{}/issues/{number}/comments?per_page={COMMENTS_PER_PAGE}&page={page}",
            gh.owner, gh.repo
        );
        let batch = fetch_github_api(&endpoint, timeout)
            .await
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        let len = batch.len();
        if len == 0 {
            break;
        }
        comments.extend(batch);
        if len < COMMENTS_PER_PAGE {
            break;
        }
        page += 1;
    }
    comments
}

fn append_comments(md: &mut String, comments: &[Value], total: i64) {
    if comments.is_empty() {
        return;
    }
    let label = if total > comments.len() as i64 {
        format!("{} of {total}", comments.len())
    } else {
        comments.len().to_string()
    };
    let _ = write!(md, "## Comments ({label})\n\n");
    for comment in comments {
        let author = login_of(comment, "user").unwrap_or_else(|| "unknown".to_string());
        let date = str_field(comment, "created_at").unwrap_or("");
        let body = str_field(comment, "body").unwrap_or("");
        let _ = write!(md, "### @{author} · {date}\n\n{body}\n\n---\n\n");
    }
}

async fn render_issue(gh: &Gh, number: i64, is_pull: bool, timeout: Duration) -> Option<String> {
    let endpoint = if is_pull {
        format!("/repos/{}/{}/pulls/{number}", gh.owner, gh.repo)
    } else {
        format!("/repos/{}/{}/issues/{number}", gh.owner, gh.repo)
    };
    let issue = fetch_github_api(&endpoint, timeout).await?;
    let title = str_field(&issue, "title").unwrap_or("");
    let state = str_field(&issue, "state").unwrap_or("");
    let author = login_of(&issue, "user").unwrap_or_else(|| "unknown".to_string());
    let mut md = format!("# {title}\n\n");
    let _ = writeln!(md, "**#{number}** · {state} · opened by @{author}");
    let _ = writeln!(
        md,
        "Created: {} · Updated: {}",
        str_field(&issue, "created_at").unwrap_or(""),
        str_field(&issue, "updated_at").unwrap_or("")
    );
    let labels = name_list(&issue, "labels");
    if !labels.is_empty() {
        let _ = writeln!(md, "Labels: {}", labels.join(", "));
    }
    md.push_str("\n---\n\n");
    md.push_str(str_field(&issue, "body").unwrap_or("*No description provided.*"));
    md.push_str("\n\n---\n\n");

    let total = i64_field(&issue, "comments");
    if total > 0 {
        let comments = fetch_issue_comments(gh, number, total, timeout).await;
        append_comments(&mut md, &comments, total);
    }
    Some(md)
}

async fn render_issues_list(gh: &Gh, timeout: Duration) -> Option<String> {
    let endpoint = format!(
        "/repos/{}/{}/issues?state=open&per_page={ISSUES_LIST_LIMIT}",
        gh.owner, gh.repo
    );
    let issues = fetch_github_api(&endpoint, timeout).await?;
    let issues = issues.as_array()?;
    let mut md = format!("# {}/{} - Open Issues\n\n", gh.owner, gh.repo);
    for issue in issues {
        if issue.get("pull_request").is_some() {
            continue;
        }
        let labels = name_list(issue, "labels");
        let label_str = if labels.is_empty() {
            String::new()
        } else {
            format!(" [{}]", labels.join(", "))
        };
        let author = login_of(issue, "user").unwrap_or_else(|| "unknown".to_string());
        let _ = writeln!(
            md,
            "- **#{}** {}{label_str}",
            i64_field(issue, "number"),
            str_field(issue, "title").unwrap_or("")
        );
        let _ = write!(
            md,
            "  by @{author} · {} comments · {}\n\n",
            i64_field(issue, "comments"),
            str_field(issue, "created_at").unwrap_or("")
        );
    }
    Some(md)
}

// --- Actions ---

fn status_label(status: &str, conclusion: Option<&str>) -> String {
    match conclusion {
        Some(c) => format!("{status} ({c})"),
        None => status.to_string(),
    }
}

fn action_duration(start: Option<&str>, end: Option<&str>) -> String {
    let (Some(start), Some(end)) = (start, end) else {
        return String::new();
    };
    let parse = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok();
    match (parse(start), parse(end)) {
        (Some(a), Some(b)) => {
            let secs = (b - a).num_seconds();
            if secs >= 0 {
                format_media_duration(secs as u64)
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

fn escape_cell(text: &str) -> String {
    text.replace('|', "\\|")
}

fn strip_log_timestamps(logs: &str) -> String {
    let no_bom = logs.strip_prefix('\u{feff}').unwrap_or(logs);
    LOG_TIMESTAMP.replace_all(no_bom, "").into_owned()
}

fn render_steps(steps: Option<&Vec<Value>>) -> String {
    let Some(steps) = steps.filter(|s| !s.is_empty()) else {
        return String::new();
    };
    let mut md = String::from("| # | Step | Status | Conclusion | Duration |\n");
    md.push_str("|---|------|--------|------------|----------|\n");
    for step in steps {
        let dur = action_duration(
            str_field(step, "started_at"),
            str_field(step, "completed_at"),
        );
        let dur = if dur.is_empty() { "-".to_string() } else { dur };
        let _ = writeln!(
            md,
            "| {} | {} | {} | {} | {dur} |",
            i64_field(step, "number"),
            escape_cell(str_field(step, "name").unwrap_or("")),
            str_field(step, "status").unwrap_or(""),
            str_field(step, "conclusion").unwrap_or("-")
        );
    }
    md.push('\n');
    md
}

fn render_run_meta(run: &Value) -> String {
    let mut md = format!(
        "**Workflow:** {}\n",
        str_field(run, "name").unwrap_or("(unknown)")
    );
    let _ = write!(md, "**Run:** #{}", i64_field(run, "run_number"));
    let attempt = i64_field(run, "run_attempt");
    if attempt > 1 {
        let _ = write!(md, " (attempt {attempt})");
    }
    let _ = writeln!(
        md,
        " · {}",
        status_label(
            str_field(run, "status").unwrap_or(""),
            str_field(run, "conclusion")
        )
    );
    if let Some(branch) = str_field(run, "head_branch") {
        let sha = str_field(run, "head_sha")
            .map(|s| format!(" @ {}", &s[..s.len().min(7)]))
            .unwrap_or_default();
        let _ = writeln!(md, "**Branch:** {branch}{sha}");
    }
    let actor = run
        .get("triggering_actor")
        .and_then(|a| str_field(a, "login"))
        .or_else(|| run.get("actor").and_then(|a| str_field(a, "login")));
    let by = actor.map(|a| format!(" · by @{a}")).unwrap_or_default();
    let _ = writeln!(
        md,
        "**Event:** {}{by}",
        str_field(run, "event").unwrap_or("")
    );
    let started = str_field(run, "run_started_at")
        .or_else(|| str_field(run, "created_at"))
        .unwrap_or("");
    let dur = action_duration(Some(started), str_field(run, "updated_at"));
    let dur = if dur.is_empty() {
        String::new()
    } else {
        format!(" · Duration: {dur}")
    };
    let _ = writeln!(md, "Started: {started}{dur}");
    let _ = writeln!(md, "URL: {}", str_field(run, "html_url").unwrap_or(""));
    md
}

async fn render_actions_run(gh: &Gh, run_id: i64, timeout: Duration) -> Option<String> {
    let run = fetch_github_api(
        &format!("/repos/{}/{}/actions/runs/{run_id}", gh.owner, gh.repo),
        timeout,
    )
    .await?;
    let title = str_field(&run, "display_title")
        .or_else(|| str_field(&run, "name"))
        .map(str::to_string)
        .unwrap_or_else(|| format!("Run #{}", i64_field(&run, "run_number")));
    let mut md = format!("# {title}\n\n{}\n---\n\n", render_run_meta(&run));

    let jobs = fetch_github_api(
        &format!(
            "/repos/{}/{}/actions/runs/{run_id}/jobs?per_page=100",
            gh.owner, gh.repo
        ),
        timeout,
    )
    .await
    .and_then(|v| v.get("jobs").and_then(Value::as_array).cloned())
    .unwrap_or_default();
    let _ = write!(md, "## Jobs ({})\n\n", jobs.len());
    for job in &jobs {
        let dur = action_duration(str_field(job, "started_at"), str_field(job, "completed_at"));
        let dur = if dur.is_empty() {
            String::new()
        } else {
            format!(" ({dur})")
        };
        let _ = write!(
            md,
            "### {} — {}{dur}\n\n",
            escape_cell(str_field(job, "name").unwrap_or("")),
            status_label(
                str_field(job, "status").unwrap_or(""),
                str_field(job, "conclusion")
            )
        );
        if str_field(job, "conclusion") != Some("success") {
            md.push_str(&render_steps(job.get("steps").and_then(Value::as_array)));
        }
    }
    Some(md)
}

async fn render_actions_job(gh: &Gh, job_id: i64, timeout: Duration) -> Option<String> {
    let job = fetch_github_api(
        &format!("/repos/{}/{}/actions/jobs/{job_id}", gh.owner, gh.repo),
        timeout,
    )
    .await?;
    let run_id = i64_field(&job, "run_id");
    let run = fetch_github_api(
        &format!("/repos/{}/{}/actions/runs/{run_id}", gh.owner, gh.repo),
        timeout,
    )
    .await;
    let name = escape_cell(str_field(&job, "name").unwrap_or(""));
    let mut md = format!("# {name}\n\n");
    append_job_context(&mut md, &job, run.as_ref());
    let dur = action_duration(
        str_field(&job, "started_at"),
        str_field(&job, "completed_at"),
    );
    let dur = if dur.is_empty() {
        String::new()
    } else {
        format!(" · {dur}")
    };
    let _ = writeln!(
        md,
        "**Job:** {name} · {}{dur}",
        status_label(
            str_field(&job, "status").unwrap_or(""),
            str_field(&job, "conclusion")
        )
    );
    if let Some(runner) = str_field(&job, "runner_name") {
        let _ = writeln!(md, "**Runner:** {runner}");
    }
    if let Some(url) = str_field(&job, "html_url") {
        let _ = writeln!(md, "URL: {url}");
    }
    md.push_str("\n---\n\n");
    let steps = render_steps(job.get("steps").and_then(Value::as_array));
    if !steps.is_empty() {
        let _ = write!(md, "## Steps\n\n{steps}");
    }
    append_job_logs(&mut md, gh, job_id, timeout).await;
    Some(md)
}

fn append_job_context(md: &mut String, job: &Value, run: Option<&Value>) {
    if let Some(run) = run {
        md.push_str(&render_run_meta(run));
    } else if let Some(workflow) = str_field(job, "workflow_name") {
        let _ = writeln!(md, "**Workflow:** {workflow}");
        if let Some(branch) = str_field(job, "head_branch") {
            let _ = writeln!(md, "**Branch:** {branch}");
        }
    }
}

async fn append_job_logs(md: &mut String, gh: &Gh, job_id: i64, timeout: Duration) {
    let url = format!(
        "https://api.github.com/repos/{}/{}/actions/jobs/{job_id}/logs",
        gh.owner, gh.repo
    );
    let mut headers = api_headers("application/vnd.github+json");
    headers.push(("X-GitHub-Api-Version".to_string(), "2022-11-28".to_string()));
    let logs = fetch_text(&url, timeout, headers).await;
    md.push_str("## Logs\n\n");
    match logs {
        Some(logs) => md.push_str(&strip_log_timestamps(&logs)),
        None => md.push_str(
            "*Logs unavailable — requires a GITHUB_TOKEN/GH_TOKEN with read access, or the run's logs have expired.*\n",
        ),
    }
}

async fn dispatch(gh: &Gh, timeout: Duration) -> Option<(String, &'static str)> {
    match &gh.target {
        Target::Blob { git_ref, path } => render_blob(gh, git_ref, path, timeout)
            .await
            .map(|md| (md, "github-raw")),
        Target::Tree { git_ref, path } => render_tree(gh, git_ref, path, timeout)
            .await
            .map(|md| (md, "github-tree")),
        Target::Commit(git_ref) => render_commit(gh, git_ref, timeout)
            .await
            .map(|md| (md, "github-commit")),
        Target::Issue(n) => render_issue(gh, *n, false, timeout)
            .await
            .map(|md| (md, "github-issue")),
        Target::Pull(n) => render_issue(gh, *n, true, timeout)
            .await
            .map(|md| (md, "github-pr")),
        Target::IssuesList => render_issues_list(gh, timeout)
            .await
            .map(|md| (md, "github-issues")),
        Target::Repo => render_repo(gh, timeout).await.map(|md| (md, "github-repo")),
        Target::ActionsRun(id) => render_actions_run(gh, *id, timeout)
            .await
            .map(|md| (md, "github-actions-run")),
        Target::ActionsJob(id) => render_actions_job(gh, *id, timeout)
            .await
            .map(|md| (md, "github-actions-job")),
    }
}

#[async_trait]
impl SpecialHandler for GitHubHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let gh = parse_github_url(url)?;
        let (md, method) = dispatch(&gh, timeout).await?;
        Some(build_result(
            &md,
            url,
            method,
            vec!["Fetched via GitHub API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(url: &str) -> Option<&'static str> {
        parse_github_url(url).map(|gh| match gh.target {
            Target::Blob { .. } => "blob",
            Target::Tree { .. } => "tree",
            Target::Commit(_) => "commit",
            Target::Issue(_) => "issue",
            Target::Pull(_) => "pull",
            Target::IssuesList => "issues",
            Target::Repo => "repo",
            Target::ActionsRun(_) => "actions-run",
            Target::ActionsJob(_) => "actions-job",
        })
    }

    #[test]
    fn parse_recognizes_url_shapes() {
        assert_eq!(kind("https://github.com/rust-lang/rust"), Some("repo"));
        assert_eq!(
            kind("https://github.com/rust-lang/rust/blob/master/src/lib.rs"),
            Some("blob")
        );
        assert_eq!(kind("https://github.com/o/r/commit/abc123"), Some("commit"));
        assert_eq!(kind("https://github.com/o/r/issues/42"), Some("issue"));
        assert_eq!(kind("https://github.com/o/r/issues"), Some("issues"));
        assert_eq!(kind("https://github.com/o/r/pull/7"), Some("pull"));
        assert_eq!(
            kind("https://github.com/o/r/actions/runs/99"),
            Some("actions-run")
        );
        assert_eq!(
            kind("https://github.com/o/r/actions/runs/99/job/5"),
            Some("actions-job")
        );
        assert_eq!(kind("https://example.com/o/r"), None);
    }

    #[test]
    fn status_label_combines_conclusion() {
        assert_eq!(
            status_label("completed", Some("failure")),
            "completed (failure)"
        );
        assert_eq!(status_label("in_progress", None), "in_progress");
    }

    #[test]
    fn duration_between_timestamps() {
        let dur = action_duration(Some("2024-01-01T00:00:00Z"), Some("2024-01-01T00:01:30Z"));
        assert_eq!(dur, "1:30");
    }

    #[test]
    fn log_timestamps_and_bom_stripped() {
        let logs = "\u{feff}2024-01-01T00:00:00.123Z hello\n2024-01-01T00:00:01Z world\n";
        assert_eq!(strip_log_timestamps(logs), "hello\nworld\n");
    }

    #[test]
    fn base64_content_decodes() {
        let encoded = base64::engine::general_purpose::STANDARD.encode("# Title\n");
        assert_eq!(
            decode_base64_content(&encoded).as_deref(),
            Some("# Title\n")
        );
    }
}
