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
mod admin_api;
mod api;
mod audit;
mod auth;
mod backup;
mod config;
mod crypt;
mod policy;
mod providers;
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
    // The admin layer sits inside the session layer, so a handler in the admin
    // group can assume both a live session and an administrator behind it.
    let admin = admin_api::routes().layer(middleware::from_fn(admin_api::require_admin));

    let guarded = api::guarded().merge(admin).with_state(state.clone()).layer(
        middleware::from_fn_with_state(state.clone(), api::require_session),
    );

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

    let state = Arc::new(AppState::new(
        store,
        &config.secret,
        config.session_ttl_secs,
    ));
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
    const TEST_SECRET: &str = "0123456789abcdef0123456789abcdef";

    fn test_state() -> Arc<AppState> {
        Arc::new(AppState::new(
            Store::open_in_memory().expect("store"),
            TEST_SECRET,
            3600,
        ))
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

    /// Log in to an account that already exists.
    async fn logged_in_existing(state: &Arc<AppState>, email: &str) -> String {
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

    /// Send an authenticated JSON request and answer the response.
    async fn authed(
        state: &Arc<AppState>,
        method: &str,
        uri: &str,
        token: &str,
        body: Option<serde_json::Value>,
    ) -> axum::response::Response {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        let body = match body {
            Some(value) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(value.to_string())
            }
            None => Body::empty(),
        };
        app(state.clone())
            .oneshot(builder.body(body).expect("request"))
            .await
            .expect("response")
    }

    #[tokio::test]
    async fn an_ordinary_account_cannot_see_the_administration_surface() {
        // 404 rather than 403: the surface does not confirm it exists.
        let state = test_state();
        let token = logged_in(&state, "ayse@firma.com", false).await;

        let response = authed(&state, "GET", "/api/v1/admin/providers", &token, None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_administration_surface_needs_a_session_at_all() {
        let state = test_state();
        let response = app(state)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/admin/providers")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_administrator_defines_a_provider_and_a_user_receives_it() {
        let state = test_state();
        let admin = logged_in(&state, "admin@firma.com", true).await;

        let created = authed(
            &state,
            "POST",
            "/api/v1/admin/providers",
            &admin,
            Some(json!({
                "name": "openai",
                "protocol": "openai",
                "api_base": "https://api.example",
                "api_key": "key-for-openai",
                "models": ["gpt-x"]
            })),
        )
        .await;
        assert_eq!(created.status(), StatusCode::CREATED);
        let provider_id = body_json(created).await["id"]
            .as_str()
            .expect("an id")
            .to_string();

        let ayse_id =
            accounts::create_user(&state.store, "ayse@firma.com", PASSWORD, false).expect("user");
        let ayse = logged_in_existing(&state, "ayse@firma.com").await;

        // Before the assignment there is nothing to receive.
        let before = authed(&state, "GET", "/api/v1/providers", &ayse, None).await;
        assert_eq!(body_json(before).await, json!([]));

        let assigned = authed(
            &state,
            "POST",
            "/api/v1/admin/assignments",
            &admin,
            Some(json!({
                "provider_id": provider_id,
                "subject_kind": "user",
                "subject_id": ayse_id
            })),
        )
        .await;
        assert_eq!(assigned.status(), StatusCode::NO_CONTENT);

        let after = body_json(authed(&state, "GET", "/api/v1/providers", &ayse, None).await).await;
        assert_eq!(after[0]["name"], "openai");
        assert_eq!(after[0]["api_key"], "key-for-openai");
        assert_eq!(after[0]["models"][0], "gpt-x");
    }

    #[tokio::test]
    async fn the_administration_listing_never_answers_with_a_key() {
        let state = test_state();
        let admin = logged_in(&state, "admin@firma.com", true).await;
        authed(
            &state,
            "POST",
            "/api/v1/admin/providers",
            &admin,
            Some(json!({ "name": "openai", "api_key": "key-for-openai" })),
        )
        .await;

        let listed =
            body_json(authed(&state, "GET", "/api/v1/admin/providers", &admin, None).await)
                .await
                .to_string();
        assert!(!listed.contains("key-for-openai"), "a key leaked: {listed}");
    }

    #[tokio::test]
    async fn a_group_assignment_reaches_a_member_over_http() {
        let state = test_state();
        let admin = logged_in(&state, "admin@firma.com", true).await;
        let ayse_id =
            accounts::create_user(&state.store, "ayse@firma.com", PASSWORD, false).expect("user");
        let ayse = logged_in_existing(&state, "ayse@firma.com").await;

        let provider_id = body_json(
            authed(
                &state,
                "POST",
                "/api/v1/admin/providers",
                &admin,
                Some(json!({ "name": "openai", "api_key": "key-for-openai" })),
            )
            .await,
        )
        .await["id"]
            .as_str()
            .expect("an id")
            .to_string();

        let group_id = body_json(
            authed(
                &state,
                "POST",
                "/api/v1/admin/groups",
                &admin,
                Some(json!({ "name": "backend" })),
            )
            .await,
        )
        .await["id"]
            .as_str()
            .expect("an id")
            .to_string();

        authed(
            &state,
            "POST",
            "/api/v1/admin/assignments",
            &admin,
            Some(json!({
                "provider_id": provider_id,
                "subject_kind": "group",
                "subject_id": group_id
            })),
        )
        .await;
        authed(
            &state,
            "POST",
            "/api/v1/admin/memberships",
            &admin,
            Some(json!({ "user_id": ayse_id, "group_id": group_id })),
        )
        .await;

        let entitled =
            body_json(authed(&state, "GET", "/api/v1/providers", &ayse, None).await).await;
        assert_eq!(entitled[0]["name"], "openai");

        // `/me` reports the membership that made it reachable.
        let me = body_json(authed(&state, "GET", "/api/v1/me", &ayse, None).await).await;
        assert_eq!(me["groups"][0]["name"], "backend");
    }

    #[tokio::test]
    async fn assigning_a_provider_that_does_not_exist_is_refused() {
        // A stored row naming nothing would look like a working assignment.
        let state = test_state();
        let admin = logged_in(&state, "admin@firma.com", true).await;

        let response = authed(
            &state,
            "POST",
            "/api/v1/admin/assignments",
            &admin,
            Some(json!({
                "provider_id": "invented",
                "subject_kind": "user",
                "subject_id": "invented"
            })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_unknown_subject_kind_is_refused_at_the_edge() {
        let state = test_state();
        let admin = logged_in(&state, "admin@firma.com", true).await;

        let response = authed(
            &state,
            "POST",
            "/api/v1/admin/assignments",
            &admin,
            Some(json!({
                "provider_id": "x",
                "subject_kind": "team",
                "subject_id": "y"
            })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn no_policy_answers_no_content_rather_than_an_empty_object() {
        // A client has to tell "nothing configured" from "unchanged".
        let state = test_state();
        let token = logged_in(&state, "ayse@firma.com", false).await;

        let response = authed(&state, "GET", "/api/v1/policy", &token, None).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn a_policy_reaches_every_account_with_its_etag() {
        let state = test_state();
        let admin = logged_in(&state, "admin@firma.com", true).await;
        let ayse = logged_in(&state, "ayse@firma.com", false).await;

        let written = authed(
            &state,
            "PUT",
            "/api/v1/admin/policy",
            &admin,
            Some(json!({ "config": { "model": "claude-opus-4-6" } })),
        )
        .await;
        assert_eq!(written.status(), StatusCode::OK);
        let checksum = body_json(written).await["checksum"]
            .as_str()
            .expect("a checksum")
            .to_string();

        let fetched = authed(&state, "GET", "/api/v1/policy", &ayse, None).await;
        assert_eq!(fetched.status(), StatusCode::OK);
        assert_eq!(
            fetched
                .headers()
                .get(header::ETAG)
                .and_then(|v| v.to_str().ok()),
            Some(checksum.as_str())
        );
        assert_eq!(
            body_json(fetched).await["config"]["model"],
            "claude-opus-4-6"
        );
    }

    #[tokio::test]
    async fn a_client_that_already_holds_the_policy_gets_no_body() {
        let state = test_state();
        let admin = logged_in(&state, "admin@firma.com", true).await;
        let ayse = logged_in(&state, "ayse@firma.com", false).await;

        let checksum = body_json(
            authed(
                &state,
                "PUT",
                "/api/v1/admin/policy",
                &admin,
                Some(json!({ "config": { "model": "claude-opus-4-6" } })),
            )
            .await,
        )
        .await["checksum"]
            .as_str()
            .expect("a checksum")
            .to_string();

        let response = app(state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/policy")
                    .header(header::AUTHORIZATION, format!("Bearer {ayse}"))
                    .header(header::IF_NONE_MATCH, &checksum)
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn a_changed_policy_changes_the_etag() {
        let state = test_state();
        let admin = logged_in(&state, "admin@firma.com", true).await;

        let first = body_json(
            authed(
                &state,
                "PUT",
                "/api/v1/admin/policy",
                &admin,
                Some(json!({ "config": { "model": "a" } })),
            )
            .await,
        )
        .await["checksum"]
            .clone();
        let second = body_json(
            authed(
                &state,
                "PUT",
                "/api/v1/admin/policy",
                &admin,
                Some(json!({ "config": { "model": "b" } })),
            )
            .await,
        )
        .await["checksum"]
            .clone();
        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn a_policy_naming_something_to_run_is_refused_with_the_key() {
        let state = test_state();
        let admin = logged_in(&state, "admin@firma.com", true).await;

        let response = authed(
            &state,
            "PUT",
            "/api/v1/admin/policy",
            &admin,
            Some(json!({ "hooks": { "SessionStart": [] } })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let error = body_json(response).await["error"].to_string();
        assert!(error.contains("hooks"), "the key was not named: {error}");
    }

    #[tokio::test]
    async fn an_ordinary_account_cannot_write_the_policy() {
        let state = test_state();
        let ayse = logged_in(&state, "ayse@firma.com", false).await;

        let response = authed(
            &state,
            "PUT",
            "/api/v1/admin/policy",
            &ayse,
            Some(json!({ "config": { "model": "whatever" } })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    /// Upload a backup with an explicit expected version.
    async fn upload(
        state: &Arc<AppState>,
        token: &str,
        version: i64,
        settings: serde_json::Value,
    ) -> axum::response::Response {
        app(state.clone())
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/v1/settings")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::IF_MATCH, version.to_string())
                    .body(Body::from(settings.to_string()))
                    .expect("request"),
            )
            .await
            .expect("response")
    }

    #[tokio::test]
    async fn no_backup_answers_no_content() {
        let state = test_state();
        let token = logged_in(&state, "ayse@firma.com", false).await;

        let response = authed(&state, "GET", "/api/v1/settings", &token, None).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn a_backup_uploads_and_comes_back() {
        let state = test_state();
        let token = logged_in(&state, "ayse@firma.com", false).await;

        let written = upload(&state, &token, 0, json!({ "config": { "model": "m" } })).await;
        assert_eq!(written.status(), StatusCode::OK);
        assert_eq!(body_json(written).await["version"], 1);

        let read = body_json(authed(&state, "GET", "/api/v1/settings", &token, None).await).await;
        assert_eq!(read["settings"]["config"]["model"], "m");
        assert_eq!(read["version"], 1);
    }

    #[tokio::test]
    async fn a_write_without_if_match_is_refused() {
        // A client that has not read what is stored must not overwrite it.
        let state = test_state();
        let token = logged_in(&state, "ayse@firma.com", false).await;

        let response = authed(
            &state,
            "PUT",
            "/api/v1/settings",
            &token,
            Some(json!({ "a": 1 })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PRECONDITION_REQUIRED);
    }

    #[tokio::test]
    async fn a_second_machine_writing_a_stale_version_is_told_the_current_one() {
        let state = test_state();
        let token = logged_in(&state, "ayse@firma.com", false).await;

        upload(&state, &token, 0, json!({ "a": 1 })).await;
        upload(&state, &token, 1, json!({ "a": 2 })).await;

        // The second machine still believes the backup is at version 1.
        let response = upload(&state, &token, 1, json!({ "a": 3 })).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);

        let body = body_json(response).await;
        assert_eq!(body["current_version"], 2);

        // And nothing was written.
        let read = body_json(authed(&state, "GET", "/api/v1/settings", &token, None).await).await;
        assert_eq!(read["settings"], json!({ "a": 2 }));
    }

    #[tokio::test]
    async fn one_account_never_sees_another_backup() {
        let state = test_state();
        let ayse = logged_in(&state, "ayse@firma.com", false).await;
        let bora = logged_in(&state, "bora@firma.com", false).await;

        upload(&state, &ayse, 0, json!({ "secret": "ayse only" })).await;

        let response = authed(&state, "GET", "/api/v1/settings", &bora, None).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn a_user_can_remove_their_own_backup() {
        let state = test_state();
        let token = logged_in(&state, "ayse@firma.com", false).await;
        upload(&state, &token, 0, json!({ "a": 1 })).await;

        let removed = authed(&state, "DELETE", "/api/v1/settings", &token, None).await;
        assert_eq!(removed.status(), StatusCode::NO_CONTENT);

        let after = authed(&state, "GET", "/api/v1/settings", &token, None).await;
        assert_eq!(after.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn a_backup_never_answers_with_the_sealed_form() {
        // The client needs the settings, not the record they were stored in.
        let state = test_state();
        let token = logged_in(&state, "ayse@firma.com", false).await;
        upload(&state, &token, 0, json!({ "a": 1 })).await;

        let body = body_json(authed(&state, "GET", "/api/v1/settings", &token, None).await)
            .await
            .to_string();
        assert!(!body.contains("v1:"), "the sealed record leaked: {body}");
    }

    /// Read the whole audit log as an administrator.
    async fn audit_log(state: &Arc<AppState>, admin: &str) -> serde_json::Value {
        body_json(authed(state, "GET", "/api/v1/admin/audit?limit=500", admin, None).await).await
    }

    #[tokio::test]
    async fn a_refused_login_is_recorded_with_the_address_and_not_the_password() {
        let state = test_state();
        let admin = logged_in(&state, "admin@firma.com", true).await;

        app(state.clone())
            .oneshot(post_json(
                "/api/v1/login",
                json!({ "email": "NOBODY@firma.com", "password": "the wrong password" }),
            ))
            .await
            .expect("response");

        let log = audit_log(&state, &admin).await.to_string();
        assert!(
            log.contains("login.refused"),
            "the attempt was not recorded"
        );
        assert!(
            log.contains("nobody@firma.com"),
            "the address was not recorded"
        );
        assert!(
            !log.contains("the wrong password"),
            "the attempted password was kept: {log}"
        );
    }

    #[tokio::test]
    async fn no_recorded_field_carries_a_secret() {
        // One pass over every write point, checking that nothing a caller
        // supplied as a credential reaches a row.
        let state = test_state();
        let admin = logged_in(&state, "admin@firma.com", true).await;

        authed(
            &state,
            "POST",
            "/api/v1/admin/providers",
            &admin,
            Some(json!({ "name": "openai", "api_key": "key-that-must-not-be-logged" })),
        )
        .await;
        authed(
            &state,
            "POST",
            "/api/v1/admin/users",
            &admin,
            Some(json!({
                "email": "bora@firma.com",
                "password": "password-that-must-not-be-logged"
            })),
        )
        .await;
        authed(
            &state,
            "PUT",
            "/api/v1/admin/policy",
            &admin,
            Some(json!({ "config": { "model": "model-that-must-not-be-logged" } })),
        )
        .await;
        upload(
            &state,
            &admin,
            0,
            json!({ "secret": "backup-that-must-not-be-logged" }),
        )
        .await;

        let log = audit_log(&state, &admin).await.to_string();
        for secret in [
            "key-that-must-not-be-logged",
            "password-that-must-not-be-logged",
            "model-that-must-not-be-logged",
            "backup-that-must-not-be-logged",
        ] {
            assert!(!log.contains(secret), "{secret} reached the log: {log}");
        }
    }

    #[tokio::test]
    async fn an_ordinary_account_cannot_read_the_audit_log() {
        let state = test_state();
        let ayse = logged_in(&state, "ayse@firma.com", false).await;

        let response = authed(&state, "GET", "/api/v1/admin/audit", &ayse, None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn taking_a_provider_key_is_recorded() {
        // The whole point of the log: who took what, and when.
        let state = test_state();
        let admin = logged_in(&state, "admin@firma.com", true).await;
        let ayse = logged_in(&state, "ayse@firma.com", false).await;

        authed(&state, "GET", "/api/v1/providers", &ayse, None).await;

        let log = audit_log(&state, &admin).await;
        let actions: Vec<&str> = log
            .as_array()
            .expect("an array")
            .iter()
            .filter_map(|entry| entry["action"].as_str())
            .collect();
        assert!(
            actions.contains(&"providers.fetch"),
            "the fetch was not recorded: {actions:?}"
        );
    }

    #[tokio::test]
    async fn a_settings_conflict_is_recorded_as_well_as_a_write() {
        let state = test_state();
        let admin = logged_in(&state, "admin@firma.com", true).await;
        let ayse = logged_in(&state, "ayse@firma.com", false).await;

        upload(&state, &ayse, 0, json!({ "a": 1 })).await;
        upload(&state, &ayse, 0, json!({ "a": 2 })).await;

        let log = audit_log(&state, &admin).await.to_string();
        assert!(log.contains("settings.write"), "the write was not recorded");
        assert!(
            log.contains("settings.conflict"),
            "two machines fighting over one account was not recorded"
        );
    }

    #[tokio::test]
    async fn an_expired_session_no_longer_opens_a_guarded_route() {
        let state = Arc::new(AppState::new(
            Store::open_in_memory().expect("store"),
            TEST_SECRET,
            -1,
        ));
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
