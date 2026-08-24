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

mod config;
mod store;

use std::sync::Arc;

use axum::Router;
use tracing::info;

use config::Config;
use store::Store;

/// Build the API router.
///
/// Split out so tests drive it with `tower::ServiceExt::oneshot` and no
/// socket. Routes that need a session go behind an auth layer applied to the
/// whole surface rather than per route, so a route added later cannot be left
/// unguarded by accident.
pub fn app(store: Arc<Store>) -> Router {
    Router::new()
        // Liveness sits outside any auth layer so a health check needs no
        // credential.
        .route("/healthz", axum::routing::get(healthz))
        .with_state(store)
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().any(|arg| arg == "--health-check") {
        return run_health_check(&config::bind_from_env()).await;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mikmik_server=info".into()),
        )
        .init();

    let config = Config::from_env()?;
    let store = Arc::new(Store::open(&config.db_path)?);

    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    info!(
        bind = %config.bind,
        db = %config.db_path.display(),
        "server listening; it does not terminate TLS, so put it behind a reverse proxy"
    );

    axum::serve(listener, app(store))
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
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_app() -> Router {
        app(Arc::new(Store::open_in_memory().expect("store")))
    }

    #[tokio::test]
    async fn health_needs_no_credential() {
        let response = test_app()
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
}
