//! The web client, embedded in the binary.
//!
//! Embedding rather than serving a directory keeps the image a single file,
//! removes a runtime path to get wrong, and leaves no filesystem to traverse.
//!
//! These routes sit outside the auth layer on purpose: the page has to load
//! before the user can enter a token, and it carries no secret of its own. The
//! API it talks to is still behind the layer.

use std::sync::LazyLock;

use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

const INDEX_HTML: &str = include_str!("../static/index.html");
const APP_JS: &str = include_str!("../static/app.js");
const STYLE_CSS: &str = include_str!("../static/style.css");

/// Cache headers for the assets.
///
/// `no-cache` rather than a long max-age: the three files are a few kilobytes,
/// and a stale client after a relay upgrade would talk to an API it no longer
/// matches.
const CACHE: &str = "no-cache";

/// Content-hash ETag for each asset, computed once at startup. The tag changes
/// when the embedded bytes change on rebuild, so a relay upgrade is picked up,
/// while an unchanged build revalidates to a bodyless 304.
static INDEX_ETAG: LazyLock<String> = LazyLock::new(|| sha256_etag(INDEX_HTML));
static APP_JS_ETAG: LazyLock<String> = LazyLock::new(|| sha256_etag(APP_JS));
static STYLE_CSS_ETAG: LazyLock<String> = LazyLock::new(|| sha256_etag(STYLE_CSS));

/// A strong ETag: the SHA-256 of the body, hex, quoted per RFC 7232. Strong is
/// correct here because no compression middleware sits between the handler and
/// the wire, so the bytes hashed are the bytes sent.
fn sha256_etag(body: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("\"{}\"", hex::encode(Sha256::digest(body.as_bytes())))
}

/// Whether the request's `If-None-Match` already holds this ETag. Handles the
/// comma-separated list, `*`, and the weak `W/` prefix (weak comparison per
/// RFC 7232 section 2.3.2).
fn if_none_match(headers: &HeaderMap, etag: &str) -> bool {
    let Some(value) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let value = value.trim();
    if value == "*" {
        return true;
    }
    let want = etag.trim_start_matches("W/");
    value
        .split(',')
        .any(|candidate| candidate.trim().trim_start_matches("W/") == want)
}

/// Serve one asset, answering 304 when the client already holds this build.
fn serve(
    headers: &HeaderMap,
    body: &'static str,
    content_type: &'static str,
    etag: &str,
) -> Response {
    if if_none_match(headers, etag) {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::CACHE_CONTROL, CACHE.to_string()),
                (header::ETAG, etag.to_string()),
            ],
        )
            .into_response();
    }
    (
        [(header::ETAG, etag.to_string())],
        asset(body, content_type),
    )
        .into_response()
}

pub fn routes<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
}

fn asset(body: &'static str, content_type: &'static str) -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (header::CACHE_CONTROL, HeaderValue::from_static(CACHE)),
            // The page loads only its own two assets and talks only to its own
            // origin, so it can afford a policy this tight.
            (
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(
                    "default-src 'none'; script-src 'self'; style-src 'self'; \
                     connect-src 'self'; base-uri 'none'; form-action 'none'; \
                     frame-ancestors 'none'",
                ),
            ),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
        ],
        body,
    )
        .into_response()
}

async fn index(headers: HeaderMap) -> Response {
    serve(
        &headers,
        INDEX_HTML,
        "text/html; charset=utf-8",
        &INDEX_ETAG,
    )
}

async fn app_js(headers: HeaderMap) -> Response {
    serve(
        &headers,
        APP_JS,
        "text/javascript; charset=utf-8",
        &APP_JS_ETAG,
    )
}

async fn style_css(headers: HeaderMap) -> Response {
    serve(
        &headers,
        STYLE_CSS,
        "text/css; charset=utf-8",
        &STYLE_CSS_ETAG,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn get_path(path: &str) -> (StatusCode, String, String) {
        let router: Router = routes();
        let response = router
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("router responds");

        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = response
            .into_body()
            .collect()
            .await
            .expect("body collects")
            .to_bytes();

        (
            status,
            content_type,
            String::from_utf8_lossy(&body).into_owned(),
        )
    }

    #[tokio::test]
    async fn the_page_is_served_without_a_token() {
        let (status, content_type, body) = get_path("/").await;

        assert_eq!(status, StatusCode::OK);
        assert!(content_type.starts_with("text/html"));
        assert!(body.contains("mikmik relay"));
    }

    #[tokio::test]
    async fn the_script_is_served_as_javascript() {
        let (status, content_type, _) = get_path("/app.js").await;

        assert_eq!(status, StatusCode::OK);
        assert!(content_type.starts_with("text/javascript"));
    }

    #[tokio::test]
    async fn the_stylesheet_is_served_as_css() {
        let (status, content_type, _) = get_path("/style.css").await;

        assert_eq!(status, StatusCode::OK);
        assert!(content_type.starts_with("text/css"));
    }

    async fn head_value(path: &str, header_name: header::HeaderName) -> String {
        let router: Router = routes();
        let response = router
            .oneshot(
                Request::builder()
                    .uri(path)
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("router responds");
        response
            .headers()
            .get(header_name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }

    #[tokio::test]
    async fn a_first_request_carries_a_quoted_etag_and_revalidates() {
        let etag = head_value("/app.js", header::ETAG).await;
        assert!(
            etag.starts_with('"') && etag.ends_with('"'),
            "etag is not quoted: {etag}"
        );
        assert_eq!(
            head_value("/app.js", header::CACHE_CONTROL).await,
            "no-cache"
        );
    }

    #[tokio::test]
    async fn a_known_etag_is_answered_304_without_a_body() {
        let etag = head_value("/style.css", header::ETAG).await;

        for candidate in [etag.clone(), format!("\"wrong\", {etag}"), "*".to_string()] {
            let router: Router = routes();
            let response = router
                .oneshot(
                    Request::builder()
                        .uri("/style.css")
                        .header(header::IF_NONE_MATCH, &candidate)
                        .body(Body::empty())
                        .expect("request builds"),
                )
                .await
                .expect("router responds");

            assert_eq!(
                response.status(),
                StatusCode::NOT_MODIFIED,
                "If-None-Match {candidate} was not a hit"
            );
            let body = response
                .into_body()
                .collect()
                .await
                .expect("body collects")
                .to_bytes();
            assert!(body.is_empty(), "304 carried a body for {candidate}");
        }
    }

    /// The page must never reach for a remote script or an inline one; that is
    /// the whole reason the policy is this narrow.
    #[tokio::test]
    async fn the_page_carries_a_content_security_policy() {
        let router: Router = routes();
        let response = router
            .oneshot(
                Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .expect("request builds"),
            )
            .await
            .expect("router responds");

        let policy = response
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();

        assert!(policy.contains("script-src 'self'"));
        assert!(policy.contains("frame-ancestors 'none'"));
    }
}
