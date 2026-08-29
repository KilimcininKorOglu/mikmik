// GitLab handler: renders a repo, file, tree, issue, or merge request from the
// gitlab.com v4 API.

use super::util::{
    build_result, format_iso_date, format_number, html_to_markdown, load_page, LoadOptions,
    RenderResult,
};
use super::SpecialHandler;
use async_trait::async_trait;
use serde_json::Value;
use std::fmt::Write;
use std::time::Duration;

pub struct GitLabHandler;

const TREE_PAGE_SIZE: usize = 100;

enum Target {
    Repo,
    Blob { git_ref: String, path: String },
    Tree { git_ref: String, path: String },
    Issue(i64),
    MergeRequest(i64),
}

struct GitLab {
    namespace: String,
    project: String,
    target: Target,
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// Parse a gitlab.com URL into a namespace/project plus a typed target.
fn parse_gitlab_url(url: &str) -> Option<GitLab> {
    let parsed = url::Url::parse(url).ok()?;
    if parsed.host_str()? != "gitlab.com" {
        return None;
    }
    let segments: Vec<&str> = parsed.path().split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        return None;
    }
    let namespace = segments[0].to_string();
    let project = segments[1].to_string();
    let rest = &segments[2..];
    let target = parse_target(rest)?;
    Some(GitLab {
        namespace,
        project,
        target,
    })
}

fn parse_target(rest: &[&str]) -> Option<Target> {
    if rest.is_empty() {
        return Some(Target::Repo);
    }
    if rest[0] != "-" || rest.len() < 2 {
        return None;
    }
    let kind = rest[1];
    let remaining = &rest[2..];
    match kind {
        "blob" if remaining.len() >= 2 => Some(Target::Blob {
            git_ref: remaining[0].to_string(),
            path: remaining[1..].join("/"),
        }),
        "tree" if !remaining.is_empty() => Some(Target::Tree {
            git_ref: remaining[0].to_string(),
            path: remaining[1..].join("/"),
        }),
        "issues" if remaining.len() == 1 => remaining[0].parse().ok().map(Target::Issue),
        "merge_requests" if remaining.len() == 1 => {
            remaining[0].parse().ok().map(Target::MergeRequest)
        }
        _ => None,
    }
}

async fn fetch_json(url: &str, timeout: Duration) -> Option<Value> {
    let result = load_page(
        url,
        LoadOptions {
            timeout,
            ..Default::default()
        },
    )
    .await;
    result
        .ok
        .then(|| serde_json::from_str(&result.content).ok())?
}

async fn fetch_text(url: &str, timeout: Duration) -> Option<String> {
    let result = load_page(
        url,
        LoadOptions {
            timeout,
            ..Default::default()
        },
    )
    .await;
    result.ok.then_some(result.content)
}

fn encoded_project_path(gl: &GitLab) -> String {
    super::util::percent_encode_component(&format!("{}/{}", gl.namespace, gl.project))
}

async fn project_id(gl: &GitLab, timeout: Duration) -> Option<i64> {
    let api = format!(
        "https://gitlab.com/api/v4/projects/{}",
        encoded_project_path(gl)
    );
    fetch_json(&api, timeout)
        .await
        .and_then(|d| d.get("id").and_then(Value::as_i64))
}

fn author_line(item: &Value) -> String {
    let author = item.get("author").cloned().unwrap_or(Value::Null);
    let name = str_field(&author, "name").unwrap_or("unknown");
    let username = str_field(&author, "username").unwrap_or("");
    format!("{name} (@{username})")
}

fn assignee_names(item: &Value) -> Vec<String> {
    item.get("assignees")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| str_field(a, "name").map(str::to_string))
        .collect()
}

fn label_list_key(item: &Value, key: &str) -> Vec<String> {
    item.get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|l| l.as_str().map(str::to_string))
        .collect()
}

async fn render_repo(gl: &GitLab, timeout: Duration) -> Option<String> {
    let api = format!(
        "https://gitlab.com/api/v4/projects/{}",
        encoded_project_path(gl)
    );
    let repo = fetch_json(&api, timeout).await?;
    let name = str_field(&repo, "name").unwrap_or(&gl.project);
    let mut md = format!("# {name}\n\n");
    if let Some(description) = str_field(&repo, "description") {
        let _ = write!(md, "{description}\n\n");
    }
    let stars = repo.get("star_count").and_then(Value::as_u64).unwrap_or(0);
    let forks = repo.get("forks_count").and_then(Value::as_u64).unwrap_or(0);
    let issues = repo
        .get("open_issues_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let _ = writeln!(
        md,
        "**Stars:** {} · **Forks:** {} · **Issues:** {}",
        format_number(stars),
        format_number(forks),
        format_number(issues)
    );
    let _ = writeln!(
        md,
        "**Visibility:** {} · **Default Branch:** {}",
        str_field(&repo, "visibility").unwrap_or("unknown"),
        str_field(&repo, "default_branch").unwrap_or("unknown")
    );
    let topics = label_list_key(&repo, "topics");
    if !topics.is_empty() {
        let _ = writeln!(md, "**Topics:** {}", topics.join(", "));
    }
    let _ = write!(
        md,
        "**Created:** {} · **Last Activity:** {}\n\n",
        format_iso_date(str_field(&repo, "created_at").unwrap_or("")),
        format_iso_date(str_field(&repo, "last_activity_at").unwrap_or(""))
    );
    if let Some(readme_url) = str_field(&repo, "readme_url") {
        if let Some(readme) = fetch_text(readme_url, timeout).await {
            if !readme.trim().is_empty() {
                let _ = write!(md, "---\n\n## README\n\n{readme}\n");
            }
        }
    }
    Some(md)
}

async fn render_file(id: i64, git_ref: &str, path: &str, timeout: Duration) -> Option<String> {
    let encoded = super::util::percent_encode_component(path);
    let api = format!(
        "https://gitlab.com/api/v4/projects/{id}/repository/files/{encoded}/raw?ref={git_ref}"
    );
    fetch_text(&api, timeout).await
}

async fn render_tree(id: i64, git_ref: &str, path: &str, timeout: Duration) -> Option<String> {
    let api = format!(
        "https://gitlab.com/api/v4/projects/{id}/repository/tree?ref={git_ref}&path={path}&per_page={TREE_PAGE_SIZE}"
    );
    let tree = fetch_json(&api, timeout).await?;
    let entries = tree.as_array()?;
    let display_path = if path.is_empty() { "/" } else { path };
    let mut md = format!("# Directory: {display_path}\n\n**Ref:** {git_ref}\n\n");
    let dirs: Vec<&Value> = entries
        .iter()
        .filter(|e| str_field(e, "type") == Some("tree"))
        .collect();
    let files: Vec<&Value> = entries
        .iter()
        .filter(|e| str_field(e, "type") == Some("blob"))
        .collect();
    if !dirs.is_empty() {
        let _ = write!(md, "## Directories ({})\n\n", dirs.len());
        for dir in dirs {
            let _ = writeln!(md, "- 📁 {}/", str_field(dir, "name").unwrap_or(""));
        }
        md.push('\n');
    }
    if !files.is_empty() {
        let _ = write!(md, "## Files ({})\n\n", files.len());
        for file in files {
            let _ = writeln!(md, "- 📄 {}", str_field(file, "name").unwrap_or(""));
        }
    }
    Some(md)
}

fn description_block(item: &Value) -> String {
    match str_field(item, "description") {
        Some(desc) => html_to_markdown(desc),
        None => "*No description*".to_string(),
    }
}

async fn render_issue(id: i64, issue_id: i64, timeout: Duration) -> Option<String> {
    let api = format!("https://gitlab.com/api/v4/projects/{id}/issues/{issue_id}");
    let issue = fetch_json(&api, timeout).await?;
    let title = str_field(&issue, "title").unwrap_or("");
    let state = str_field(&issue, "state").unwrap_or("").to_uppercase();
    let mut md = format!("# Issue #{issue_id}: {title}\n\n");
    let _ = writeln!(
        md,
        "**State:** {state} · **Author:** {}",
        author_line(&issue)
    );
    let _ = writeln!(
        md,
        "**Created:** {} · **Updated:** {}",
        format_iso_date(str_field(&issue, "created_at").unwrap_or("")),
        format_iso_date(str_field(&issue, "updated_at").unwrap_or(""))
    );
    let up = issue.get("upvotes").and_then(Value::as_i64).unwrap_or(0);
    let down = issue.get("downvotes").and_then(Value::as_i64).unwrap_or(0);
    let notes = issue
        .get("user_notes_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let _ = writeln!(
        md,
        "**Upvotes:** {up} · **Downvotes:** {down} · **Comments:** {notes}"
    );
    let labels = label_list_key(&issue, "labels");
    if !labels.is_empty() {
        let _ = writeln!(md, "**Labels:** {}", labels.join(", "));
    }
    let assignees = assignee_names(&issue);
    if !assignees.is_empty() {
        let _ = writeln!(md, "**Assignees:** {}", assignees.join(", "));
    }
    let _ = write!(
        md,
        "\n---\n\n## Description\n\n{}",
        description_block(&issue)
    );
    Some(md)
}

async fn render_mr(id: i64, mr_id: i64, timeout: Duration) -> Option<String> {
    let api = format!("https://gitlab.com/api/v4/projects/{id}/merge_requests/{mr_id}");
    let mr = fetch_json(&api, timeout).await?;
    let title = str_field(&mr, "title").unwrap_or("");
    let state = str_field(&mr, "state").unwrap_or("").to_uppercase();
    let mut md = format!("# MR !{mr_id}: {title}\n\n");
    if mr.get("draft").and_then(Value::as_bool) == Some(true) {
        md.push_str("**[DRAFT]** ");
    }
    let _ = writeln!(md, "**State:** {state} · **Author:** {}", author_line(&mr));
    let _ = writeln!(
        md,
        "**Branch:** {} → {}",
        str_field(&mr, "source_branch").unwrap_or(""),
        str_field(&mr, "target_branch").unwrap_or("")
    );
    let _ = writeln!(
        md,
        "**Created:** {} · **Updated:** {}",
        format_iso_date(str_field(&mr, "created_at").unwrap_or("")),
        format_iso_date(str_field(&mr, "updated_at").unwrap_or(""))
    );
    let up = mr.get("upvotes").and_then(Value::as_i64).unwrap_or(0);
    let down = mr.get("downvotes").and_then(Value::as_i64).unwrap_or(0);
    let notes = mr
        .get("user_notes_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let _ = writeln!(
        md,
        "**Merge Status:** {} · **Upvotes:** {up} · **Downvotes:** {down} · **Comments:** {notes}",
        str_field(&mr, "merge_status").unwrap_or("")
    );
    let labels = label_list_key(&mr, "labels");
    if !labels.is_empty() {
        let _ = writeln!(md, "**Labels:** {}", labels.join(", "));
    }
    let assignees = assignee_names(&mr);
    if !assignees.is_empty() {
        let _ = writeln!(md, "**Assignees:** {}", assignees.join(", "));
    }
    let _ = write!(md, "\n---\n\n## Description\n\n{}", description_block(&mr));
    Some(md)
}

async fn dispatch(gl: &GitLab, timeout: Duration) -> Option<(String, &'static str)> {
    match &gl.target {
        Target::Repo => render_repo(gl, timeout).await.map(|md| (md, "gitlab-repo")),
        Target::Blob { git_ref, path } => {
            let id = project_id(gl, timeout).await?;
            render_file(id, git_ref, path, timeout)
                .await
                .map(|md| (md, "gitlab-raw"))
        }
        Target::Tree { git_ref, path } => {
            let id = project_id(gl, timeout).await?;
            render_tree(id, git_ref, path, timeout)
                .await
                .map(|md| (md, "gitlab-tree"))
        }
        Target::Issue(issue_id) => {
            let id = project_id(gl, timeout).await?;
            render_issue(id, *issue_id, timeout)
                .await
                .map(|md| (md, "gitlab-issue"))
        }
        Target::MergeRequest(mr_id) => {
            let id = project_id(gl, timeout).await?;
            render_mr(id, *mr_id, timeout)
                .await
                .map(|md| (md, "gitlab-mr"))
        }
    }
}

#[async_trait]
impl SpecialHandler for GitLabHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let gl = parse_gitlab_url(url)?;
        let (md, method) = dispatch(&gl, timeout).await?;
        Some(build_result(
            &md,
            url,
            method,
            vec!["Fetched via GitLab API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn target_kind(url: &str) -> Option<&'static str> {
        parse_gitlab_url(url).map(|gl| match gl.target {
            Target::Repo => "repo",
            Target::Blob { .. } => "blob",
            Target::Tree { .. } => "tree",
            Target::Issue(_) => "issue",
            Target::MergeRequest(_) => "mr",
        })
    }

    #[test]
    fn parse_recognizes_url_shapes() {
        assert_eq!(target_kind("https://gitlab.com/ns/proj"), Some("repo"));
        assert_eq!(
            target_kind("https://gitlab.com/ns/proj/-/blob/main/src/lib.rs"),
            Some("blob")
        );
        assert_eq!(
            target_kind("https://gitlab.com/ns/proj/-/tree/main/src"),
            Some("tree")
        );
        assert_eq!(
            target_kind("https://gitlab.com/ns/proj/-/issues/42"),
            Some("issue")
        );
        assert_eq!(
            target_kind("https://gitlab.com/ns/proj/-/merge_requests/7"),
            Some("mr")
        );
        assert_eq!(target_kind("https://example.com/ns/proj"), None);
    }

    #[test]
    fn blob_captures_ref_and_path() {
        let gl = parse_gitlab_url("https://gitlab.com/ns/proj/-/blob/main/src/lib.rs").unwrap();
        match gl.target {
            Target::Blob { git_ref, path } => {
                assert_eq!(git_ref, "main");
                assert_eq!(path, "src/lib.rs");
            }
            _ => panic!("expected blob"),
        }
    }

    #[tokio::test]
    async fn issue_renders_metadata() {
        let issue = json!({
            "title": "Broken build",
            "state": "opened",
            "author": { "name": "Alice", "username": "alice" },
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-02T00:00:00Z",
            "upvotes": 3,
            "downvotes": 0,
            "user_notes_count": 5,
            "labels": ["bug"],
            "description": "<p>It <strong>fails</strong>.</p>"
        });
        // Exercise the pure rendering pieces directly.
        assert_eq!(author_line(&issue), "Alice (@alice)");
        assert_eq!(label_list_key(&issue, "labels"), vec!["bug".to_string()]);
        assert_eq!(description_block(&issue), "It **fails**.");
    }
}
