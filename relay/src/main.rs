//! mikmik-relay — a self-hosted relay between a running mikmik session and a
//! phone or browser.
//!
//! The CLI dials out and long-polls, so the developer machine needs no inbound
//! port. The relay only queues and forwards; it does not interpret prompts,
//! events or code.
//!
//! Security posture: a single shared token guards everything, and anything
//! holding it can drive a tool-capable agent on the connected machine. The
//! relay does not terminate TLS — run it behind a reverse proxy, or only on a
//! VPN or LAN.

mod auth;
mod client;
mod protocol;
mod runner;
mod state;
mod web;

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::Router;
use tracing::{info, warn};

use state::{Limits, Relay};

/// Runtime configuration, all from the environment so the container needs no
/// config file.
struct Config {
    token: String,
    bind: String,
    limits: Limits,
}

fn env_duration_secs(name: &str, default: Duration) -> Duration {
    match std::env::var(name) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(secs) if secs > 0 => Duration::from_secs(secs),
            _ => {
                warn!(name, value = %raw, "ignoring unparseable duration; using the default");
                default
            }
        },
        Err(_) => default,
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    match std::env::var(name) {
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(value) if value > 0 => value,
            _ => {
                warn!(name, value = %raw, "ignoring unparseable size; using the default");
                default
            }
        },
        Err(_) => default,
    }
}

fn load_config() -> anyhow::Result<Config> {
    let raw_token = std::env::var("RELAY_TOKEN").unwrap_or_default();
    // Refuse to start rather than run with a weak secret: this token is a
    // remote command-execution credential for the connected machine.
    let token = auth::validate_token(&raw_token)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .to_string();

    let defaults = Limits::default();
    Ok(Config {
        token,
        bind: std::env::var("RELAY_BIND").unwrap_or_else(|_| "0.0.0.0:8350".to_string()),
        limits: Limits {
            event_buffer: env_usize("RELAY_EVENT_BUFFER", defaults.event_buffer),
            inbound_queue: env_usize("RELAY_INBOUND_QUEUE", defaults.inbound_queue),
            session_ttl: env_duration_secs("RELAY_SESSION_TTL_SECS", defaults.session_ttl),
        },
    })
}

/// Reject any request that does not carry the configured bearer token.
///
/// Applied to the whole API surface rather than per route, so a route added
/// later cannot be left unguarded by accident.
async fn require_token(
    State(token): State<Arc<String>>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let headers = request.headers();
    // A browser `EventSource` cannot set request headers, so the SSE endpoint
    // is only reachable with the cookie. Both are accepted everywhere rather
    // than per route, so the two paths cannot drift apart.
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(auth::bearer_from_header)
        .or_else(|| {
            headers
                .get(header::COOKIE)
                .and_then(|value| value.to_str().ok())
                .and_then(auth::token_from_cookies)
        });

    match presented {
        Some(presented) if auth::token_matches(&token, presented) => Ok(next.run(request).await),
        Some(_) => {
            warn!(
                path = %request.uri().path(),
                "rejected a request carrying the wrong token"
            );
            Err(StatusCode::FORBIDDEN)
        }
        None => Err(StatusCode::UNAUTHORIZED),
    }
}

/// Build the API router. Split out so tests can drive it without a socket.
pub fn app(relay: Arc<Relay>, token: Arc<String>) -> Router {
    Router::new()
        .merge(runner::routes())
        .merge(client::routes())
        .with_state(relay)
        .layer(middleware::from_fn_with_state(token, require_token))
        // The API carries session data and live event streams; none of it may
        // sit in a shared or browser cache. Default every API response that set
        // no `Cache-Control` of its own to `no-store`.
        .layer(middleware::from_fn(no_store_if_absent))
        // Liveness sits outside the auth layer so a health check does not need
        // the credential.
        .route("/healthz", axum::routing::get(healthz))
        // So does the page itself: it has to load before the user can enter a
        // token, and it carries no secret of its own.
        .merge(web::routes())
}

async fn healthz() -> &'static str {
    "ok"
}

/// Default any response that set no `Cache-Control` of its own to `no-store`.
///
/// A handler that wants its response cached or revalidated sets its own header
/// and this leaves it untouched; everything else, session data and live
/// streams, is kept out of every cache.
async fn no_store_if_absent(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    response
}

/// Drop sessions whose runner has gone quiet.
async fn sweep_loop(relay: Arc<Relay>) {
    let interval = relay.limits().session_ttl / 4;
    let mut ticker = tokio::time::interval(interval.max(Duration::from_secs(5)));
    loop {
        ticker.tick().await;
        let removed = relay.sweep_expired().await;
        if removed > 0 {
            info!(removed, "swept idle sessions");
        }
    }
}

/// Address a health check should dial.
///
/// `RELAY_BIND` may be a wildcard, which is not connectable, so the host part
/// is rewritten to loopback while the port is kept.
fn health_check_target(bind: &str) -> String {
    match bind.rsplit_once(':') {
        Some((host, port)) if host.is_empty() || host == "0.0.0.0" || host == "[::]" => {
            format!("127.0.0.1:{port}")
        }
        _ => bind.to_string(),
    }
}

/// Prove the listener is up, for the container healthcheck.
///
/// A TCP connect rather than an HTTP request: it needs no client library, and
/// the runtime image carries no `curl` or `wget` to shell out to.
async fn run_health_check(bind: &str) -> anyhow::Result<()> {
    let target = health_check_target(bind);
    tokio::net::TcpStream::connect(&target)
        .await
        .map_err(|e| anyhow::anyhow!("health check could not reach {target}: {e}"))?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().any(|arg| arg == "--health-check") {
        let bind = std::env::var("RELAY_BIND").unwrap_or_else(|_| "0.0.0.0:8350".to_string());
        return run_health_check(&bind).await;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mikmik_relay=info".into()),
        )
        .init();

    let config = load_config()?;
    let relay = Arc::new(Relay::new(config.limits));

    tokio::spawn(sweep_loop(relay.clone()));

    let router = app(relay, Arc::new(config.token));
    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    info!(
        bind = %config.bind,
        "relay listening; it does not terminate TLS, so put it behind a reverse proxy or keep it on a VPN"
    );

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("shutting down");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::RegisterBody;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use http_body_util::BodyExt;
    use serde_json::json;
    use tower::ServiceExt;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn test_app() -> Router {
        app(
            Arc::new(Relay::new(Limits::default())),
            Arc::new(TOKEN.to_string()),
        )
    }

    fn authed(method: &str, uri: &str) -> axum::http::request::Builder {
        HttpRequest::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
    }

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        serde_json::from_slice(&bytes).expect("json")
    }

    #[test]
    fn a_wildcard_bind_is_dialled_on_loopback() {
        // 0.0.0.0 is bindable but not connectable, so the health check has to
        // rewrite it or it would always report the relay as down.
        assert_eq!(health_check_target("0.0.0.0:8350"), "127.0.0.1:8350");
        assert_eq!(health_check_target("[::]:8350"), "127.0.0.1:8350");
        assert_eq!(health_check_target(":8350"), "127.0.0.1:8350");
    }

    #[test]
    fn a_concrete_bind_is_dialled_as_given() {
        assert_eq!(health_check_target("127.0.0.1:9000"), "127.0.0.1:9000");
        assert_eq!(health_check_target("10.0.0.5:8350"), "10.0.0.5:8350");
    }

    #[tokio::test]
    async fn a_health_check_against_nothing_fails() {
        // Port 1 needs root to bind, so nothing is listening there.
        assert!(run_health_check("127.0.0.1:1").await.is_err());
    }

    #[tokio::test]
    async fn a_request_without_a_token_is_unauthorized() {
        let response = test_app()
            .oneshot(
                HttpRequest::builder()
                    .method("GET")
                    .uri("/api/claude_code/sessions/s1/poll")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_request_with_the_wrong_token_is_forbidden() {
        let response = test_app()
            .oneshot(
                HttpRequest::builder()
                    .method("GET")
                    .uri("/api/claude_code/sessions/s1/poll")
                    .header(
                        header::AUTHORIZATION,
                        "Bearer wrong-but-long-enough-token-x",
                    )
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn health_needs_no_token() {
        let response = test_app()
            .oneshot(
                HttpRequest::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn an_api_response_is_kept_out_of_every_cache() {
        let response = test_app()
            .oneshot(
                authed("GET", "/api/client/sessions")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store"),
        );
    }

    #[tokio::test]
    async fn liveness_is_left_out_of_the_cache_layer() {
        let response = test_app()
            .oneshot(
                HttpRequest::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        // `/healthz` sits outside the API layer, so it gets no `no-store`.
        assert_ne!(
            response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store"),
        );
    }

    #[tokio::test]
    async fn registering_then_polling_returns_a_queued_message() {
        let relay = Arc::new(Relay::new(Limits::default()));
        let router = app(relay.clone(), Arc::new(TOKEN.to_string()));

        let response = router
            .clone()
            .oneshot(
                authed("POST", "/api/claude_code/sessions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({ "session_id": "s1", "label": "work" }).to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::CREATED);

        relay
            .push_inbound("s1", protocol::BridgeMessage::Ping)
            .await;

        let response = router
            .oneshot(
                authed("GET", "/api/claude_code/sessions/s1/poll")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);

        let body = body_json(response).await;
        assert_eq!(body, json!([{ "type": "ping" }]));
    }

    #[tokio::test]
    async fn an_uploaded_event_is_retrievable_by_sequence() {
        let relay = Arc::new(Relay::new(Limits::default()));
        let router = app(relay.clone(), Arc::new(TOKEN.to_string()));

        let response = router
            .oneshot(
                authed("POST", "/api/claude_code/sessions/s1/events")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({ "events": [{ "type": "text_delta", "text": "hi" }] }).to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::ACCEPTED);

        let (events, latest) = relay.events_since("s1", 0).await.expect("session");
        assert_eq!(latest, 1);
        assert_eq!(events[0].event["text"], "hi");
    }

    #[tokio::test]
    async fn a_poll_with_nothing_queued_returns_an_empty_array() {
        let relay = Arc::new(Relay::new(Limits {
            session_ttl: Duration::from_secs(60),
            ..Limits::default()
        }));
        relay.register(&RegisterBody::new("s1")).await;
        let router = app(relay, Arc::new(TOKEN.to_string()));

        // The handler holds the request for POLL_HOLD; pause the clock so the
        // test does not actually wait 25 seconds.
        tokio::time::pause();
        let response = router
            .oneshot(
                authed("GET", "/api/claude_code/sessions/s1/poll")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await, json!([]));
    }

    #[tokio::test]
    async fn the_supplementary_message_endpoint_stays_empty() {
        // Answering here as well would deliver every prompt twice.
        let response = test_app()
            .oneshot(
                authed("GET", "/api/bridge/sessions/s1/messages")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await, json!([]));
    }

    #[tokio::test]
    async fn deregistering_removes_the_session() {
        let relay = Arc::new(Relay::new(Limits::default()));
        relay.register(&RegisterBody::new("s1")).await;
        let router = app(relay.clone(), Arc::new(TOKEN.to_string()));

        let response = router
            .oneshot(
                authed("DELETE", "/api/claude_code/sessions/s1")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(!relay.exists("s1").await);
    }
}
