//! mikmik-server — a self-hosted configuration and identity server.
//!
//! An organisation runs one of these. It holds the user accounts, the provider
//! definitions and keys the organisation hands out, the settings policy it
//! enforces, and each user's own settings backup. A mikmik installation logs
//! in against it and receives what that user is entitled to.
//!
//! Security posture: the server does not terminate TLS. Run it behind a
//! reverse proxy. `MIKMIK_SERVER_SECRET` encrypts the stored provider keys and
//! settings blobs and derives the session tokens, so it is treated as the key
//! to everything the database holds rather than a convenience.

mod accounts;
mod admin;
mod api;
mod auth;
mod config;
mod state;
mod store;

use std::sync::Arc;

use axum::middleware;
use axum::Router;
use tracing::info;

use config::Config;
use state::AppState;
use store::Store;

/// Build the API router.
///
/// Split out so tests drive it with `tower::ServiceExt::oneshot` and no
/// socket. The session layer covers the guarded group as a whole rather than
/// route by route, so a route added there later cannot be left unguarded by
/// accident.
pub fn app(state: Arc<AppState>) -> Router {
    let guarded = api::guarded()
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            api::require_session,
        ));

    let public = api::public().with_state(state);

    guarded
        .merge(public)
        // Liveness sits outside the session layer so a health check needs no
        // credential.
        .route("/healthz", axum::routing::get(healthz))
}

async fn healthz() -> &'static str {
    "ok"
}

/// Prove the listener is up, for the container healthcheck.
///
/// A TCP connect rather than an HTTP request: it needs no client library, and
/// the runtime image carries no `curl` or `wget` to shell out to.
async fn run_health_check(bind: &str) -> anyhow::Result<()> {
    let target = config::health_check_target(bind);
    tokio::net::TcpStream::connect(&target)
        .await
        .map_err(|e| anyhow::anyhow!("health check could not reach {target}: {e}"))?;
    Ok(())
}

/// Drop sessions whose time has passed.
async fn sweep_loop(state: Arc<AppState>) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(3600));
    loop {
        ticker.tick().await;
        match accounts::sweep_expired_sessions(&state.store) {
            Ok(removed) if removed > 0 => info!(removed, "swept expired sessions"),
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "sweeping sessions failed"),
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|arg| arg == "--health-check") {
        return run_health_check(&config::bind_from_env()).await;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mikmik_server=info".into()),
        )
        .init();

    let config = Config::from_env()?;
    let store = Store::open(&config.db_path)?;

    // `admin` runs against the same database and then exits, so an operator
    // can open the first account without the server listening.
    if args.get(1).map(String::as_str) == Some("admin") {
        return admin::run(&store, &args[2..]);
    }

    let state = Arc::new(AppState::new(store, config.session_ttl_secs));
    tokio::spawn(sweep_loop(state.clone()));

    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    info!(
        bind = %config.bind,
        db = %config.db_path.display(),
        "server listening; it does not terminate TLS, so put it behind a reverse proxy"
    );
    if !accounts::any_user_exists(&state.store)? {
        info!("no accounts yet; open the first one with `mikmik-server admin create <email>`");
    }

    axum::serve(listener, app(state))
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
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::json;
    use tower::ServiceExt;

    const PASSWORD: &str = "correct horse battery";

    fn test_state() -> Arc<AppState> {
        Arc::new(AppState::new(Store::open_in_memory().expect("store"), 3600))
    }

    async fn body_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    }

    fn post_json(uri: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .expect("request")
    }

    /// Open an account and log in, answering the token.
    async fn logged_in(state: &Arc<AppState>, email: &str, is_admin: bool) -> String {
        accounts::create_user(&state.store, email, PASSWORD, is_admin).expect("created");
        let response = app(state.clone())
            .oneshot(post_json(
                "/api/v1/login",
                json!({ "email": email, "password": PASSWORD }),
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        body_json(response).await["token"]
            .as_str()
            .expect("a token")
            .to_string()
    }

    #[tokio::test]
    async fn health_needs_no_credential() {
        let response = app(test_state())
            .oneshot(
                Request::builder()
                    .uri("/healthz")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn a_health_check_against_nothing_fails() {
        // Port 1 needs root to bind, so nothing is listening there.
        assert!(run_health_check("127.0.0.1:1").await.is_err());
    }

    #[tokio::test]
    async fn a_guarded_route_without_a_token_is_unauthorized() {
        let response = app(test_state())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/me")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_guarded_route_with_an_invented_token_is_unauthorized() {
        let response = app(test_state())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/me")
                    .header(header::AUTHORIZATION, "Bearer invented")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_wrong_password_is_rejected() {
        let state = test_state();
        accounts::create_user(&state.store, "ayse@firma.com", PASSWORD, false).expect("created");

        let response = app(state)
            .oneshot(post_json(
                "/api/v1/login",
                json!({ "email": "ayse@firma.com", "password": "wrong password here" }),
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_unknown_address_answers_exactly_what_a_wrong_password_answers() {
        // Telling the two apart would make this endpoint a way to find out who
        // works here.
        let state = test_state();
        accounts::create_user(&state.store, "ayse@firma.com", PASSWORD, false).expect("created");

        let wrong_password = app(state.clone())
            .oneshot(post_json(
                "/api/v1/login",
                json!({ "email": "ayse@firma.com", "password": "wrong password here" }),
            ))
            .await
            .expect("response");
        let unknown = app(state)
            .oneshot(post_json(
                "/api/v1/login",
                json!({ "email": "nobody@firma.com", "password": "wrong password here" }),
            ))
            .await
            .expect("response");

        assert_eq!(wrong_password.status(), unknown.status());
        assert_eq!(body_json(wrong_password).await, body_json(unknown).await);
    }

    #[tokio::test]
    async fn a_disabled_account_cannot_log_in() {
        let state = test_state();
        let id = accounts::create_user(&state.store, "ayse@firma.com", PASSWORD, false)
            .expect("created");
        state
            .store
            .with(|conn| {
                conn.execute(
                    "UPDATE users SET disabled_at = ?1 WHERE id = ?2",
                    rusqlite::params![accounts::now_secs(), id],
                )
            })
            .expect("disabled");

        let response = app(state)
            .oneshot(post_json(
                "/api/v1/login",
                json!({ "email": "ayse@firma.com", "password": PASSWORD }),
            ))
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn logging_in_sets_a_locked_down_cookie() {
        let state = test_state();
        accounts::create_user(&state.store, "ayse@firma.com", PASSWORD, false).expect("created");

        let response = app(state)
            .oneshot(post_json(
                "/api/v1/login",
                json!({ "email": "ayse@firma.com", "password": PASSWORD }),
            ))
            .await
            .expect("response");

        let cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .expect("a cookie");
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
    }

    #[tokio::test]
    async fn a_session_reaches_a_guarded_route_by_header_and_by_cookie() {
        let state = test_state();
        let token = logged_in(&state, "ayse@firma.com", true).await;

        for (name, value) in [
            (header::AUTHORIZATION, format!("Bearer {token}")),
            (header::COOKIE, format!("{}={token}", auth::COOKIE_NAME)),
        ] {
            let response = app(state.clone())
                .oneshot(
                    Request::builder()
                        .uri("/api/v1/me")
                        .header(name.clone(), value)
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::OK, "{name} was not accepted");

            let body = body_json(response).await;
            assert_eq!(body["email"], "ayse@firma.com");
            assert_eq!(body["is_admin"], true);
        }
    }

    #[tokio::test]
    async fn me_never_answers_with_a_password_hash() {
        let state = test_state();
        let token = logged_in(&state, "ayse@firma.com", false).await;

        let response = app(state)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/me")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        let body = body_json(response).await.to_string();
        assert!(!body.contains("argon2"), "the stored hash leaked: {body}");
        assert!(
            !body.contains("password"),
            "a password field leaked: {body}"
        );
    }

    #[tokio::test]
    async fn logging_out_ends_the_session() {
        let state = test_state();
        let token = logged_in(&state, "ayse@firma.com", false).await;

        let response = app(state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/logout")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let after = app(state)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/me")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(after.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_expired_session_no_longer_opens_a_guarded_route() {
        let state = Arc::new(AppState::new(Store::open_in_memory().expect("store"), -1));
        let token = logged_in(&state, "ayse@firma.com", false).await;

        let response = app(state)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/me")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
