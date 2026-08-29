// Sourcegraph handler: renders a repo, file, or code search via the
// sourcegraph.com GraphQL API.

use super::util::{build_result, load_page, LoadOptions, RenderResult};
use super::SpecialHandler;
use serde_json::{json, Value};
use std::fmt::Write;
use std::time::Duration;

use async_trait::async_trait;

pub struct SourcegraphHandler;

const GRAPHQL_ENDPOINT: &str = "https://sourcegraph.com/.api/graphql";
const MAX_RESULTS: usize = 10;
const MAX_LINE_MATCHES: usize = 5;

const REPO_QUERY: &str = "query Repo($name: String!) { repository(name: $name) { name url description defaultBranch { name } } }";
const REPO_FILE_QUERY: &str = "query RepoFile($name: String!, $path: String!, $rev: String!) { repository(name: $name) { name url description defaultBranch { name } commit(rev: $rev) { blob(path: $path) { content } } } }";
const SEARCH_QUERY: &str = "query Search($query: String!) { search(query: $query, version: V2) { results { results { __typename ... on FileMatch { repository { name url } file { path url } lineMatches { preview lineNumber } } ... on Repository { name url } } matchCount limitHit } } }";

enum Target {
    Search(String),
    Repo {
        name: String,
    },
    File {
        name: String,
        rev: Option<String>,
        path: String,
    },
}

fn str_field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn parse_search(parsed: &url::Url) -> Option<Target> {
    let query = parsed
        .query_pairs()
        .find(|(k, _)| k == "q")
        .map(|(_, v)| v.trim().to_string())
        .filter(|q| !q.is_empty())?;
    Some(Target::Search(query))
}

/// Split the trailing repo segment on `@` into a bare name and an optional rev.
fn split_rev(last: &str) -> (String, Option<String>) {
    match last.find('@') {
        Some(idx) if idx > 0 => {
            let rev = &last[idx + 1..];
            (
                last[..idx].to_string(),
                (!rev.is_empty()).then(|| rev.to_string()),
            )
        }
        _ => (last.to_string(), None),
    }
}

fn parse_repo_or_file(parsed: &url::Url) -> Option<Target> {
    let parts: Vec<String> = parsed
        .path()
        .split('/')
        .filter(|s| !s.is_empty())
        .map(super::util::percent_decode)
        .collect();
    if parts.len() < 3 {
        return None;
    }
    let hyphen = parts.iter().position(|p| p == "-");
    let mut repo_parts: Vec<String> = match hyphen {
        Some(idx) => parts[..idx].to_vec(),
        None => parts.clone(),
    };
    if repo_parts.len() < 3 {
        return None;
    }
    let last = repo_parts.pop()?;
    let (tail, rev) = split_rev(&last);
    repo_parts.push(tail);
    let name = repo_parts.join("/");

    if let Some(idx) = hyphen {
        if parts.get(idx + 1).map(String::as_str) == Some("blob") {
            let path = parts[idx + 2..].join("/");
            if path.is_empty() {
                return None;
            }
            return Some(Target::File { name, rev, path });
        }
    }
    Some(Target::Repo { name })
}

fn parse_sourcegraph_url(url: &str) -> Option<Target> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    if host != "sourcegraph.com" && host != "www.sourcegraph.com" {
        return None;
    }
    if parsed.path().starts_with("/search") {
        return parse_search(&parsed);
    }
    parse_repo_or_file(&parsed)
}

/// POST a GraphQL query and return its `data` object, or `None` on any error.
async fn fetch_graphql(query: &str, variables: Value, timeout: Duration) -> Option<Value> {
    let body = json!({ "query": query, "variables": variables }).to_string();
    let result = load_page(
        GRAPHQL_ENDPOINT,
        LoadOptions {
            timeout,
            method: reqwest::Method::POST,
            headers: vec![
                ("Accept".to_string(), "application/json".to_string()),
                ("Content-Type".to_string(), "application/json".to_string()),
            ],
            body: Some(body),
        },
    )
    .await;
    if !result.ok {
        return None;
    }
    let parsed: Value = serde_json::from_str(&result.content).ok()?;
    if parsed
        .get("errors")
        .and_then(Value::as_array)
        .is_some_and(|e| !e.is_empty())
    {
        return None;
    }
    parsed.get("data").cloned().filter(|d| !d.is_null())
}

fn format_repo(repo: &Value) -> String {
    let name = str_field(repo, "name").unwrap_or("unknown");
    let mut md = format!("# {name}\n\n");
    if let Some(description) = str_field(repo, "description") {
        let _ = write!(md, "{description}\n\n");
    }
    let _ = writeln!(md, "**URL:** {}", str_field(repo, "url").unwrap_or(""));
    if let Some(branch) = repo.get("defaultBranch").and_then(|b| str_field(b, "name")) {
        let _ = writeln!(md, "**Default branch:** {branch}");
    }
    md
}

async fn render_repo(name: &str, timeout: Duration) -> Option<String> {
    let data = fetch_graphql(REPO_QUERY, json!({ "name": name }), timeout).await?;
    let repo = data.get("repository").filter(|r| !r.is_null())?;
    Some(format_repo(repo))
}

async fn render_file(name: &str, path: &str, rev: &str, timeout: Duration) -> Option<String> {
    let variables = json!({ "name": name, "path": path, "rev": rev });
    let data = fetch_graphql(REPO_FILE_QUERY, variables, timeout).await?;
    let repo = data.get("repository").filter(|r| !r.is_null())?;
    let content = repo
        .get("commit")
        .and_then(|c| c.get("blob"))
        .and_then(|b| b.get("content"))
        .and_then(Value::as_str)?;
    let mut md = format!("{}\n", format_repo(repo));
    let _ = write!(md, "**Path:** {path}\n**Revision:** {rev}\n\n");
    let _ = write!(md, "---\n\n## File\n\n```text\n{content}\n```\n");
    Some(md)
}

fn append_file_match(md: &mut String, result: &Value) {
    let repo = result.get("repository").cloned().unwrap_or(Value::Null);
    let file = result.get("file").cloned().unwrap_or(Value::Null);
    let repo_name = str_field(&repo, "name").unwrap_or("unknown");
    let file_path = str_field(&file, "path").unwrap_or("unknown");
    let _ = write!(md, "### {repo_name}/{file_path}\n\n");
    if let Some(url) = str_field(&repo, "url") {
        let _ = writeln!(md, "**Repository:** {url}");
    }
    if let Some(url) = str_field(&file, "url") {
        let _ = writeln!(md, "**File:** {url}");
    }
    let lines = result.get("lineMatches").and_then(Value::as_array);
    let Some(lines) = lines.filter(|l| !l.is_empty()) else {
        return;
    };
    md.push_str("\n```text\n");
    for line in lines.iter().take(MAX_LINE_MATCHES) {
        let preview = line
            .get("preview")
            .and_then(Value::as_str)
            .unwrap_or("")
            .replace('\n', " ");
        let number = line.get("lineNumber").and_then(Value::as_i64).unwrap_or(0);
        let _ = writeln!(md, "L{number}: {}", preview.trim());
    }
    md.push_str("```\n\n");
}

fn append_search_result(md: &mut String, result: &Value) {
    match str_field(result, "__typename") {
        Some("FileMatch") => append_file_match(md, result),
        Some("Repository") => {
            let name = str_field(result, "name").unwrap_or("unknown");
            let _ = write!(md, "### {name}\n\n");
            if let Some(url) = str_field(result, "url") {
                let _ = writeln!(md, "**Repository:** {url}");
            }
            md.push('\n');
        }
        _ => {}
    }
}

async fn render_search(query: &str, timeout: Duration) -> Option<String> {
    let data = fetch_graphql(SEARCH_QUERY, json!({ "query": query }), timeout).await?;
    let results_data = data
        .get("search")
        .and_then(|s| s.get("results"))
        .filter(|r| !r.is_null())?;
    let results = results_data
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut md = format!("# Sourcegraph Search\n\n**Query:** `{query}`\n");
    if let Some(count) = results_data.get("matchCount").and_then(Value::as_i64) {
        let _ = writeln!(md, "**Matches:** {count}");
    }
    if let Some(hit) = results_data.get("limitHit").and_then(Value::as_bool) {
        let _ = writeln!(md, "**Limit hit:** {}", if hit { "yes" } else { "no" });
    }
    md.push('\n');
    if results.is_empty() {
        md.push_str("_No results._\n");
        return Some(md);
    }
    md.push_str("## Results\n\n");
    for result in results.iter().take(MAX_RESULTS) {
        append_search_result(&mut md, result);
    }
    if results.len() > MAX_RESULTS {
        let _ = writeln!(md, "[…{} results elided…]", results.len() - MAX_RESULTS);
    }
    Some(md)
}

async fn dispatch(target: Target, timeout: Duration) -> Option<(String, &'static str)> {
    match target {
        Target::Search(query) => render_search(&query, timeout)
            .await
            .map(|md| (md, "sourcegraph-search")),
        Target::File { name, rev, path } => {
            let rev = rev.unwrap_or_else(|| "HEAD".to_string());
            render_file(&name, &path, &rev, timeout)
                .await
                .map(|md| (md, "sourcegraph-file"))
        }
        Target::Repo { name } => render_repo(&name, timeout)
            .await
            .map(|md| (md, "sourcegraph-repo")),
    }
}

#[async_trait]
impl SpecialHandler for SourcegraphHandler {
    async fn handle(&self, url: &str, timeout: Duration) -> Option<RenderResult> {
        let target = parse_sourcegraph_url(url)?;
        let (md, method) = dispatch(target, timeout).await?;
        Some(build_result(
            &md,
            url,
            method,
            vec!["Fetched via Sourcegraph GraphQL API".to_string()],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn kind(url: &str) -> Option<&'static str> {
        parse_sourcegraph_url(url).map(|t| match t {
            Target::Search(_) => "search",
            Target::Repo { .. } => "repo",
            Target::File { .. } => "file",
        })
    }

    #[test]
    fn parse_recognizes_targets() {
        assert_eq!(
            kind("https://sourcegraph.com/search?q=fn+main"),
            Some("search")
        );
        assert_eq!(
            kind("https://sourcegraph.com/github.com/rust-lang/rust"),
            Some("repo")
        );
        assert_eq!(
            kind("https://sourcegraph.com/github.com/rust-lang/rust/-/blob/src/lib.rs"),
            Some("file")
        );
        assert_eq!(kind("https://example.com/x"), None);
    }

    #[test]
    fn file_target_captures_rev_and_path() {
        let target =
            parse_sourcegraph_url("https://sourcegraph.com/github.com/a/b@v1.0/-/blob/src/main.rs")
                .unwrap();
        match target {
            Target::File { name, rev, path } => {
                assert_eq!(name, "github.com/a/b");
                assert_eq!(rev.as_deref(), Some("v1.0"));
                assert_eq!(path, "src/main.rs");
            }
            _ => panic!("expected file"),
        }
    }

    #[test]
    fn repo_markdown_lays_out_fields() {
        let repo = json!({
            "name": "github.com/rust-lang/rust",
            "url": "https://sourcegraph.com/github.com/rust-lang/rust",
            "description": "The Rust language",
            "defaultBranch": { "name": "master" }
        });
        let md = format_repo(&repo);
        assert!(md.contains("# github.com/rust-lang/rust"));
        assert!(md.contains("The Rust language"));
        assert!(md.contains("**Default branch:** master"));
    }

    #[test]
    fn file_match_renders_line_previews() {
        let result = json!({
            "__typename": "FileMatch",
            "repository": { "name": "github.com/a/b", "url": "https://x.test/repo" },
            "file": { "path": "src/lib.rs", "url": "https://x.test/file" },
            "lineMatches": [{ "preview": "fn main() {}", "lineNumber": 10 }]
        });
        let mut md = String::new();
        append_search_result(&mut md, &result);
        assert!(md.contains("### github.com/a/b/src/lib.rs"));
        assert!(md.contains("L10: fn main() {}"));
    }
}
