//! The HTTP surface.
//!
//! Routes split into two groups. `guarded` needs a live session; `public` does
//! not, and holds only the login endpoint. The session layer is applied to the
//! whole guarded group rather than per route, so a route added to it later
//! cannot be left unguarded by accident.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::accounts::{self, User};
use crate::auth;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// Routing
// ---------------------------------------------------------------------------

/// Routes that need a live session.
pub fn guarded() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/v1/me", get(me))
        .route("/api/v1/logout", post(logout))
        .route("/api/v1/providers", get(entitled_providers))
        .route("/api/v1/policy", get(policy))
}

/// Routes that must be reachable without one.
pub fn public() -> Router<Arc<AppState>> {
    Router::new().route("/api/v1/login", post(login))
}

// ---------------------------------------------------------------------------
// The session layer
// ---------------------------------------------------------------------------

/// Reject any request that does not carry a live session.
///
/// A browser cannot set a header on every navigation, so the cookie is
/// accepted alongside the bearer token. Both are accepted everywhere rather
/// than per route, so the two paths cannot drift apart.
pub async fn require_session(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let presented = presented_token(request.headers()).map(str::to_string);
    // Not the check that rejects an anonymous request: the lookup below finds
    // nothing for a token that is not there and answers 401 either way. This
    // only spares the database a query per unauthenticated request.
    let Some(token) = presented else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    match accounts::session_user(&state.store, &token) {
        Ok(Some(user)) => {
            request.extensions_mut().insert(SessionUser {
                user,
                token: token.clone(),
            });
            Ok(next.run(request).await)
        }
        Ok(None) => Err(StatusCode::UNAUTHORIZED),
        Err(error) => {
            warn!(%error, "the session lookup failed");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// The token a request carries, from either place it may live.
fn presented_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(auth::bearer_from_header)
        .or_else(|| {
            headers
                .get(header::COOKIE)
                .and_then(|value| value.to_str().ok())
                .and_then(auth::token_from_cookies)
        })
}

/// What the layer hands to a guarded handler.
#[derive(Debug, Clone)]
pub struct SessionUser {
    pub user: User,
    pub token: String,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct LoginBody {
    pub email: String,
    pub password: String,
}

#[derive(Debug, Serialize)]
struct LoginResponse {
    token: String,
    expires_in: i64,
    user: UserView,
}

#[derive(Debug, Serialize)]
struct UserView {
    id: String,
    email: String,
    is_admin: bool,
}

impl From<&User> for UserView {
    fn from(user: &User) -> Self {
        Self {
            id: user.id.clone(),
            email: user.email.clone(),
            is_admin: user.is_admin,
        }
    }
}

/// `/me`, which adds the groups the account belongs to.
#[derive(Debug, Serialize)]
struct MeView {
    id: String,
    email: String,
    is_admin: bool,
    groups: Vec<crate::providers::Group>,
}

/// Exchange an address and a password for a session token.
///
/// Every failure answers 401 with the same body. A wrong password, an unknown
/// address and a disabled account are indistinguishable from outside, because
/// telling them apart would turn this endpoint into a way to enumerate who
/// works here.
async fn login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> Response {
    let found = match accounts::find_by_email(&state.store, &body.email) {
        Ok(found) => found,
        Err(error) => {
            warn!(%error, "the account lookup failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    // Verify against a decoy when the address is unknown, so the argon2 work
    // happens either way and the reply time does not answer "does this address
    // exist" on its own.
    let (user, stored) = match found {
        Some((user, hash)) => (Some(user), hash),
        None => (None, auth::decoy_hash().to_string()),
    };
    let password_ok = auth::verify_password(&body.password, &stored);

    let Some(user) = user.filter(|user| password_ok && !user.disabled) else {
        return rejected_login();
    };

    let token = match accounts::open_session(&state.store, &user.id, state.session_ttl_secs) {
        Ok(token) => token,
        Err(error) => {
            warn!(%error, "opening the session failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let cookie = auth::session_cookie(
        &token,
        auth::is_secure_request(&headers),
        state.session_ttl_secs,
    );
    let body = LoginResponse {
        expires_in: state.session_ttl_secs,
        user: UserView::from(&user),
        token,
    };
    ([(header::SET_COOKIE, cookie)], Json(body)).into_response()
}

fn rejected_login() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": "the email or password is wrong" })),
    )
        .into_response()
}

/// Who the caller is, and which groups they belong to.
async fn me(
    State(state): State<Arc<AppState>>,
    axum::Extension(session): axum::Extension<SessionUser>,
) -> Response {
    let groups = match crate::providers::groups_for_user(&state.store, &session.user.id) {
        Ok(groups) => groups,
        Err(error) => {
            warn!(%error, "listing the groups failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    Json(MeView {
        id: session.user.id.clone(),
        email: session.user.email.clone(),
        is_admin: session.user.is_admin,
        groups,
    })
    .into_response()
}

/// The organisation's settings policy, with the checksum as its `ETag`.
///
/// A client that already holds this version sends it back as `If-None-Match`
/// and receives 304 with no body, which is what makes an hourly poll cheap.
/// No policy at all answers 204, so a client can tell "nothing configured"
/// from "unchanged" without guessing.
async fn policy(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let stored = match crate::policy::get(&state.store) {
        Ok(stored) => stored,
        Err(error) => {
            warn!(%error, "reading the policy failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let Some(stored) = stored else {
        return StatusCode::NO_CONTENT.into_response();
    };

    let known = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .map(str::trim);
    if known == Some(stored.checksum.as_str()) {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, stored.checksum)]).into_response();
    }

    ([(header::ETAG, stored.checksum)], Json(stored.settings)).into_response()
}

/// Every provider this account may use, with its key.
///
/// The key is what makes the entitlement real: a provider nobody assigned is
/// not merely hidden from the client, it has no credential to be used with.
async fn entitled_providers(
    State(state): State<Arc<AppState>>,
    axum::Extension(session): axum::Extension<SessionUser>,
) -> Response {
    match crate::providers::entitled_for_user(&state.store, &state.sealer, &session.user.id) {
        Ok(rows) => Json(rows).into_response(),
        Err(error) => {
            warn!(%error, "listing the entitled providers failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// End this session.
async fn logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    axum::Extension(session): axum::Extension<SessionUser>,
) -> Response {
    if let Err(error) = accounts::close_session(&state.store, &session.token) {
        warn!(%error, "closing the session failed");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let cookie = auth::cleared_cookie(auth::is_secure_request(&headers));
    (StatusCode::NO_CONTENT, [(header::SET_COOKIE, cookie)]).into_response()
}
