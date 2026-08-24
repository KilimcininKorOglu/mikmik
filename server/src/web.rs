//! The administration page, embedded in the binary.
//!
//! Embedding rather than serving a directory keeps the image a single file,
//! removes a runtime path to get wrong, and leaves no filesystem to traverse.
//!
//! These routes sit outside the session layer on purpose: the page has to load
//! before anyone can sign in, and it carries no secret of its own. Everything
//! it reads is behind the layer, so an anonymous visitor sees an empty form and
//! nothing else. What the page may do once signed in is decided by the API,
//! never by which elements the page chose to draw.

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

const INDEX_HTML: &str = include_str!("../static/index.html");
const APP_JS: &str = include_str!("../static/app.js");
const STYLE_CSS: &str = include_str!("../static/style.css");

/// Cache headers for the assets.
///
/// `no-cache` rather than a long max-age: the three files are a few kilobytes,
/// and a stale page after an upgrade would talk to an API it no longer matches.
const CACHE: &str = "no-cache";

/// What the page is allowed to do.
///
/// `script-src 'self'` with no `unsafe-inline` is what makes the page safe to
/// build from values an administrator typed: an injected `<script>` has no way
/// to run. `form-action 'none'` means a form whose script failed to load cannot
/// submit anywhere at all, rather than posting a password as a query string.
const POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
                      connect-src 'self'; base-uri 'none'; form-action 'none'; \
                      frame-ancestors 'none'";

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
            (
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(POLICY),
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

async fn index() -> Response {
    asset(INDEX_HTML, "text/html; charset=utf-8")
}

async fn app_js() -> Response {
    asset(APP_JS, "text/javascript; charset=utf-8")
}

async fn style_css() -> Response {
    asset(STYLE_CSS, "text/css; charset=utf-8")
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
                    .expect("request"),
            )
            .await
            .expect("response");

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
            .expect("body")
            .to_bytes();

        (
            status,
            content_type,
            String::from_utf8_lossy(&body).into_owned(),
        )
    }

    #[tokio::test]
    async fn the_page_is_served_without_a_session() {
        let (status, content_type, body) = get_path("/").await;

        assert_eq!(status, StatusCode::OK);
        assert!(content_type.starts_with("text/html"));
        assert!(body.contains("mikmik server"));
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

    #[tokio::test]
    async fn every_asset_carries_the_policy_and_nosniff() {
        for path in ["/", "/index.html", "/app.js", "/style.css"] {
            let router: Router = routes();
            let response = router
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");

            let policy = response
                .headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();
            assert!(policy.contains("script-src 'self'"), "{path}");
            assert!(policy.contains("frame-ancestors 'none'"), "{path}");
            assert!(
                !policy.contains("unsafe-inline"),
                "{path} would run an injected script"
            );
            assert_eq!(
                response
                    .headers()
                    .get(header::X_CONTENT_TYPE_OPTIONS)
                    .and_then(|value| value.to_str().ok()),
                Some("nosniff"),
                "{path}"
            );
        }
    }

    /// The page holds no state of its own between loads. Anything it kept in
    /// the browser would be readable by any script that ever reached the page,
    /// while the session cookie is `HttpOnly` and is not.
    #[tokio::test]
    async fn the_page_stores_nothing_in_the_browser() {
        let (_, _, script) = get_path("/app.js").await;
        assert!(
            !script.contains("localStorage") && !script.contains("sessionStorage"),
            "the page keeps state in the browser"
        );
    }

    /// Everything the page draws from a value an administrator typed goes
    /// through `textContent`. One `innerHTML` here would turn a provider named
    /// `<img onerror=...>` into script running in an administrator's session.
    #[tokio::test]
    async fn the_page_never_builds_markup_from_a_value() {
        let (_, _, script) = get_path("/app.js").await;
        for sink in [
            "innerHTML",
            "outerHTML",
            "insertAdjacentHTML",
            "document.write",
        ] {
            assert!(!script.contains(sink), "the page uses {sink}");
        }
    }
}
