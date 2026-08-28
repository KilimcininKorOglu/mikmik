// TinyFish provider: a SERP-backed search API.
//
// Available when `TINYFISH_API_KEY` is set. GET with the query and filters as
// query parameters, paged until the result cap is met. `site:` directives map
// onto include/exclude domain lists, and a `lang:` directive onto TinyFish's
// location/language parameters.

use crate::web::search::provider::{Recency, SearchParams, SearchProvider};
use crate::web::search::query::{format_query, QuerySyntax};
use crate::web::search::types::{
    SearchProviderError, SearchProviderId, SearchResponse, SearchSource,
};
use crate::ToolContext;
use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;

pub struct TinyFishProvider;

const TINYFISH_SEARCH_URL: &str = "https://api.search.tinyfish.ai";
const DEFAULT_PAGE_SIZE: usize = 10;
const MAX_PAGE: usize = 10;

fn api_key() -> Option<String> {
    super::stored_or_env_key(SearchProviderId::Tinyfish, "TINYFISH_API_KEY")
}

fn recency_minutes(recency: Recency) -> u32 {
    match recency {
        Recency::Day => 1440,
        Recency::Week => 10080,
        Recency::Month => 43200,
        Recency::Year => 525_600,
    }
}

/// Bare hosts from `site:` values; path constraints stay centrally post-filtered.
fn site_hosts(sites: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut hosts = Vec::new();
    for site in sites {
        if let Some(host) = site.split('/').next().filter(|h| !h.is_empty()) {
            if seen.insert(host.to_string()) {
                hosts.push(host.to_string());
            }
        }
    }
    hosts
}

static LOCALE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^([a-z]{2})(?:[-_]([a-z]{2}))?(?:[-_]|$)").expect("static locale regex")
});

/// TinyFish `language` (ISO 639-1) and optional `location` (ISO 3166-1) from a
/// `lang:` directive. `lang:it` → language only, `lang:it-it` → both.
fn locale(lang: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(lang) = lang else {
        return (None, None);
    };
    let lowered = lang.to_lowercase();
    let Some(m) = LOCALE.captures(&lowered) else {
        return (None, None);
    };
    let language = m.get(1).map(|g| g.as_str().to_string());
    let location = m.get(2).map(|g| g.as_str().to_uppercase());
    (language, location)
}

/// The fixed request shape derived once from the params.
struct Request {
    query: String,
    recency_minutes: Option<u32>,
    include_domains: Vec<String>,
    exclude_domains: Vec<String>,
    location: Option<String>,
    language: Option<String>,
    page_size: usize,
}

fn build_request(params: &SearchParams<'_>) -> Request {
    let parsed = params.parsed;
    let syntax = QuerySyntax {
        phrases: true,
        negation: true,
        filetype: true,
        ..QuerySyntax::default()
    };
    let query = if parsed.has_directives {
        format_query(parsed, syntax)
    } else {
        params.query.to_string()
    };
    let (include_domains, exclude_domains) = if parsed.has_directives {
        (
            site_hosts(&parsed.sites),
            site_hosts(&parsed.excluded_sites),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    let (language, location) = locale(parsed.lang.as_deref());
    Request {
        query,
        recency_minutes: params.recency.map(recency_minutes),
        include_domains,
        exclude_domains,
        location,
        language,
        page_size: params.limit.clamp(1, DEFAULT_PAGE_SIZE),
    }
}

fn build_url(req: &Request, page: usize) -> Result<url::Url, SearchProviderError> {
    let mut url = url::Url::parse(TINYFISH_SEARCH_URL)
        .map_err(|e| SearchProviderError::new(SearchProviderId::Tinyfish, format!("URL: {e}")))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("query", &req.query);
        q.append_pair("num_results", &req.page_size.to_string());
        q.append_pair("page", &page.to_string());
        if let Some(minutes) = req.recency_minutes {
            q.append_pair("recency_minutes", &minutes.to_string());
        }
        if !req.include_domains.is_empty() {
            q.append_pair("include_domains", &req.include_domains.join(","));
        }
        if !req.exclude_domains.is_empty() {
            q.append_pair("exclude_domains", &req.exclude_domains.join(","));
        }
        if let Some(location) = &req.location {
            q.append_pair("location", location);
        }
        if let Some(language) = &req.language {
            q.append_pair("language", language);
        }
    }
    Ok(url)
}

/// Append results from one page, deduping by URL. Returns the page length.
fn append_sources(sources: &mut Vec<SearchSource>, results: &[Value], seen: &mut HashSet<String>) {
    for result in results {
        let Some(url) = result.get("url").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if url.is_empty() || !seen.insert(url.to_string()) {
            continue;
        }
        let site_name = result
            .get("site_name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let title = result
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or(site_name)
            .unwrap_or(url);
        let snippet = result
            .get("snippet")
            .and_then(Value::as_str)
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|s| !s.is_empty());
        sources.push(SearchSource {
            title: title.to_string(),
            url: url.to_string(),
            snippet,
            author: site_name.map(str::to_string),
            ..Default::default()
        });
    }
}

#[async_trait]
impl SearchProvider for TinyFishProvider {
    fn id(&self) -> SearchProviderId {
        SearchProviderId::Tinyfish
    }

    async fn is_available(&self, _ctx: &ToolContext) -> bool {
        api_key().is_some()
    }

    async fn search(
        &self,
        params: &SearchParams<'_>,
    ) -> Result<SearchResponse, SearchProviderError> {
        let key = api_key()
            .ok_or_else(|| SearchProviderError::new(self.id(), "TINYFISH_API_KEY is not set"))?;
        let client = reqwest::Client::builder()
            .timeout(params.timeout)
            .build()
            .map_err(|e| SearchProviderError::new(self.id(), format!("HTTP client: {e}")))?;
        let req = build_request(params);

        let mut sources: Vec<SearchSource> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for page in 0..=MAX_PAGE {
            if sources.len() >= params.limit {
                break;
            }
            let url = build_url(&req, page)?;
            let resp = client
                .get(url)
                .header("Accept", "application/json")
                .header("X-API-Key", &key)
                .send()
                .await
                .map_err(|e| {
                    SearchProviderError::new(self.id(), format!("Search request failed: {e}"))
                })?;
            if !resp.status().is_success() {
                let status = resp.status().as_u16();
                return Err(SearchProviderError::with_status(
                    self.id(),
                    format!("TinyFish API returned status {status}"),
                    status,
                ));
            }
            let data: Value = resp.json().await.map_err(|e| {
                SearchProviderError::new(self.id(), format!("Failed to parse response: {e}"))
            })?;
            let Some(results) = data.get("results").and_then(Value::as_array) else {
                return Err(SearchProviderError::new(
                    self.id(),
                    "TinyFish returned an unexpected response shape",
                ));
            };
            append_sources(&mut sources, results, &mut seen);
            if results.len() < req.page_size {
                break;
            }
        }
        sources.truncate(params.limit);

        let mut response = SearchResponse::empty(self.id().as_str());
        response.sources = sources;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recency_maps_to_minutes() {
        assert_eq!(recency_minutes(Recency::Day), 1440);
        assert_eq!(recency_minutes(Recency::Year), 525_600);
    }

    #[test]
    fn locale_splits_language_and_region() {
        assert_eq!(locale(Some("it")), (Some("it".into()), None));
        assert_eq!(
            locale(Some("it-IT")),
            (Some("it".into()), Some("IT".into()))
        );
        // A script subtag (`hans`) is not a region, so no location is derived.
        assert_eq!(locale(Some("zh-hans")), (Some("zh".into()), None));
        assert_eq!(locale(None), (None, None));
    }

    #[test]
    fn site_hosts_dedupes_and_strips_paths() {
        let hosts = site_hosts(&["github.com/a".into(), "github.com/b".into(), "x.io".into()]);
        assert_eq!(hosts, vec!["github.com", "x.io"]);
    }

    #[test]
    fn append_dedupes_by_url_and_collapses_snippet_whitespace() {
        let results = vec![
            json!({ "url": "https://a", "title": "A", "snippet": "one   two" }),
            json!({ "url": "https://a", "title": "dup" }),
            json!({ "url": "https://b", "site_name": "Site B" }),
        ];
        let mut sources = Vec::new();
        let mut seen = HashSet::new();
        append_sources(&mut sources, &results, &mut seen);
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].snippet.as_deref(), Some("one two"));
        // Missing title falls back to site_name.
        assert_eq!(sources[1].title, "Site B");
        assert_eq!(sources[1].author.as_deref(), Some("Site B"));
    }

    #[test]
    fn the_url_carries_query_and_filters() {
        let req = Request {
            query: "rust".into(),
            recency_minutes: Some(1440),
            include_domains: vec!["docs.rs".into()],
            exclude_domains: vec![],
            location: Some("IT".into()),
            language: Some("it".into()),
            page_size: 10,
        };
        let url = build_url(&req, 2).expect("url");
        let s = url.as_str();
        assert!(s.contains("query=rust"));
        assert!(s.contains("recency_minutes=1440"));
        assert!(s.contains("include_domains=docs.rs"));
        assert!(s.contains("location=IT"));
        assert!(s.contains("page=2"));
    }
}
