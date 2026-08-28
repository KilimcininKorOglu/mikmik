// Unified web-search pipeline: a provider registry walked as an auto-fallback
// chain, plus a structured query layer that parses and enforces Google-style
// directives (`site:`, `filetype:`, `before:`/`after:`, …).

pub mod provider;
pub mod providers;
pub mod query;
pub mod types;

use crate::{ToolContext, ToolResult};
use provider::{provider_for, resolve_provider_candidates, Recency, SearchParams};
use query::{apply_query_constraints, parse_search_query};
use std::time::Duration;
use types::{SearchProviderId, SearchResponse, DEFAULT_WEB_SEARCH_TIMEOUT_SECONDS};

/// One recorded provider failure while walking the chain.
struct Failure {
    id: SearchProviderId,
    message: String,
}

/// Run a web search across the auto-fallback provider chain.
///
/// `limit` is the already-clamped result count and `recency` the parsed
/// temporal filter. Walks [`resolve_provider_candidates`], skipping
/// unimplemented and unavailable providers, and returns the first provider's
/// renderable result. When `config.web_search_fallback` is off, a provider
/// failure stops the search instead of falling through.
pub async fn execute_search(
    query: &str,
    limit: usize,
    recency: Option<Recency>,
    ctx: &ToolContext,
) -> ToolResult {
    let parsed = parse_search_query(query);
    let timeout = Duration::from_secs(DEFAULT_WEB_SEARCH_TIMEOUT_SECONDS);
    let fallback_enabled = ctx.config.web_search_fallback;

    let mut failures: Vec<Failure> = Vec::new();
    let mut available_count = 0usize;

    for candidate in resolve_provider_candidates(&[]) {
        let Some(provider) = provider_for(candidate.id) else {
            continue;
        };
        if !provider.is_available(ctx).await {
            continue;
        }
        available_count += 1;

        let params = SearchParams {
            query,
            parsed: &parsed,
            limit,
            recency,
            timeout,
            ctx,
        };
        match provider.search(&params).await {
            Ok(response) => {
                if let Some(result) = render_response(response, &parsed) {
                    return result;
                }
                // No renderable content: treat as a failure and fall through.
                failures.push(Failure {
                    id: candidate.id,
                    message: format!("{} returned no renderable content", candidate.id.label()),
                });
            }
            Err(error) => failures.push(Failure {
                id: candidate.id,
                message: error.message,
            }),
        }

        if !fallback_enabled {
            break;
        }
    }

    finish(failures, available_count, fallback_enabled)
}

/// Apply the lenient constraint filter, then render, or `None` when the
/// response carries nothing worth showing.
fn render_response(
    mut response: SearchResponse,
    parsed: &query::StructuredQuery,
) -> Option<ToolResult> {
    let mut notes: Vec<String> = Vec::new();
    if parsed.has_constraints && !response.sources.is_empty() {
        let filtered = apply_query_constraints(&response.sources, parsed);
        response.sources = filtered.sources;
        for label in filtered.dropped {
            notes.push(format!(
                "no results matched `{label}`; the constraint was relaxed"
            ));
        }
    }
    if !response.has_renderable_content() {
        return None;
    }
    Some(ToolResult::success(format_for_llm(&response, &notes)))
}

/// Build the error result when the chain produced nothing.
fn finish(failures: Vec<Failure>, available_count: usize, fallback_enabled: bool) -> ToolResult {
    if available_count == 0 && failures.is_empty() {
        return ToolResult::error(
            "No web search provider configured. Point SEARXNG_URL at a SearXNG instance, or set \
             TAVILY_API_KEY or BRAVE_SEARCH_API_KEY.",
        );
    }
    let message = match failures.len() {
        0 => "Unknown error from web search provider".to_string(),
        1 => failures[0].message.clone(),
        _ => format!(
            "All web search providers failed: {}",
            failures
                .iter()
                .map(|f| format!("{}: {}", f.id.label(), f.message))
                .collect::<Vec<_>>()
                .join("; ")
        ),
    };
    let hint = if !fallback_enabled && failures.len() == 1 {
        "\n\nSet \"webSearchFallback\": true in settings.json to let WebSearch continue with \
         another provider when the first one fails."
    } else {
        ""
    };
    ToolResult::error(format!("{message}{hint}"))
}

/// Render a `SearchResponse` for the model. `notes` (relaxed-constraint
/// warnings) lead the output. Ported from oh-my-pi `formatForLLM`.
fn format_for_llm(response: &SearchResponse, notes: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for note in notes {
        parts.push(format!("Note: {note}"));
    }

    if let Some(answer) = &response.answer {
        parts.push(answer.clone());
        if !response.sources.is_empty() {
            parts.push("\n## Sources".to_string());
            parts.push(count_label("source", response.sources.len()));
        }
    }

    for (i, src) in response.sources.iter().enumerate() {
        let age = source_age(src);
        let age_part = age.map(|a| format!(" ({a})")).unwrap_or_default();
        parts.push(format!(
            "[{}] {}{}\n    {}",
            i + 1,
            src.title,
            age_part,
            src.url
        ));
        if let Some(snippet) = &src.snippet {
            parts.push(format!("    {}", truncate(snippet, 240)));
        }
    }

    if !response.citations.is_empty() {
        parts.push("\n## Citations".to_string());
        parts.push(count_label("citation", response.citations.len()));
        for (i, citation) in response.citations.iter().enumerate() {
            let title = if citation.title.is_empty() {
                &citation.url
            } else {
                &citation.title
            };
            parts.push(format!("[{}] {}\n    {}", i + 1, title, citation.url));
            if let Some(cited) = &citation.cited_text {
                parts.push(format!("    {}", truncate(cited, 240)));
            }
        }
    }

    if !response.related_questions.is_empty() {
        parts.push("\n## Related".to_string());
        parts.push(count_label("question", response.related_questions.len()));
        for q in &response.related_questions {
            parts.push(format!("- {q}"));
        }
    }

    if !response.search_queries.is_empty() {
        parts.push(format!("Search queries: {}", response.search_queries.len()));
        for query in response.search_queries.iter().take(3) {
            parts.push(format!("- {}", truncate(query, 120)));
        }
    }

    parts.join("\n")
}

fn count_label(label: &str, count: usize) -> String {
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {label}{plural}")
}

/// A human-readable age from `age_seconds`, else the raw `published_date`.
fn source_age(src: &types::SearchSource) -> Option<String> {
    if let Some(age) = src.age_seconds {
        if age.is_finite() && age >= 0.0 {
            return Some(format_age(age as u64));
        }
    }
    src.published_date.clone().filter(|d| !d.is_empty())
}

fn format_age(seconds: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 3600;
    const DAY: u64 = 86_400;
    const WEEK: u64 = 604_800;
    const MONTH: u64 = 2_592_000;
    const YEAR: u64 = 31_536_000;
    let (n, unit) = match seconds {
        s if s < HOUR => (s / MINUTE, "m"),
        s if s < DAY => (s / HOUR, "h"),
        s if s < WEEK => (s / DAY, "d"),
        s if s < MONTH => (s / WEEK, "w"),
        s if s < YEAR => (s / MONTH, "mo"),
        s => (s / YEAR, "y"),
    };
    format!("{n}{unit} ago")
}

fn truncate(text: &str, max_len: usize) -> String {
    if text.chars().count() <= max_len {
        return text.to_string();
    }
    let cut = max_len.saturating_sub(1);
    let head: String = text.chars().take(cut).collect();
    format!("{head}\u{2026}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::{SearchCitation, SearchResponse, SearchSource};

    fn source(title: &str, url: &str, snippet: &str) -> SearchSource {
        SearchSource {
            title: title.into(),
            url: url.into(),
            snippet: Some(snippet.into()),
            ..Default::default()
        }
    }

    #[test]
    fn format_renders_answer_sources_and_citations() {
        let response = SearchResponse {
            provider: "anthropic".into(),
            answer: Some("Rust guarantees memory safety.".into()),
            sources: vec![source("Ownership", "https://x/o", "rules")],
            citations: vec![SearchCitation {
                url: "https://x/c".into(),
                title: "Cited".into(),
                cited_text: Some("quoted".into()),
            }],
            ..Default::default()
        };
        let out = format_for_llm(&response, &[]);
        assert!(out.contains("Rust guarantees memory safety."));
        assert!(out.contains("## Sources"));
        assert!(out.contains("[1] Ownership\n    https://x/o"));
        assert!(out.contains("    rules"));
        assert!(out.contains("## Citations"));
        assert!(out.contains("[1] Cited\n    https://x/c"));
    }

    #[test]
    fn notes_lead_the_output() {
        let response = SearchResponse {
            provider: "brave".into(),
            sources: vec![source("A", "https://a", "s")],
            ..Default::default()
        };
        let out = format_for_llm(&response, &["constraint relaxed".to_string()]);
        assert!(out.starts_with("Note: constraint relaxed"));
    }

    #[test]
    fn count_label_pluralizes() {
        assert_eq!(count_label("source", 1), "1 source");
        assert_eq!(count_label("source", 3), "3 sources");
    }

    #[test]
    fn format_age_reads_naturally() {
        assert_eq!(format_age(30), "0m ago");
        assert_eq!(format_age(3 * 86_400), "3d ago");
        assert_eq!(format_age(40 * 86_400), "1mo ago");
    }

    #[test]
    fn truncate_caps_at_the_limit_with_an_ellipsis() {
        assert_eq!(truncate("abc", 10), "abc");
        let long = "x".repeat(300);
        let cut = truncate(&long, 240);
        assert_eq!(cut.chars().count(), 240);
        assert!(cut.ends_with('\u{2026}'));
    }
}
