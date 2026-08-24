//! The administration surface.
//!
//! Every route here sits behind `require_admin`, which sits inside the session
//! layer, so a handler in this module can assume both a live session and an
//! administrator behind it.

use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tracing::warn;

use crate::accounts;
use crate::api::SessionUser;
use crate::providers::{self, ProviderInput, SubjectKind};
use crate::state::AppState;

/// Routes that need an administrator.
pub fn routes() -> Router<Arc<AppState>> {
    Router::new()
        .route(
            "/api/v1/admin/providers",
            get(list_providers).post(create_provider),
        )
        .route("/api/v1/admin/providers/{id}", delete(delete_provider))
        .route("/api/v1/admin/groups", get(list_groups).post(create_group))
        .route("/api/v1/admin/groups/{id}", delete(delete_group))
        .route("/api/v1/admin/memberships", post(add_membership))
        .route("/api/v1/admin/memberships/remove", post(remove_membership))
        .route("/api/v1/admin/assignments", post(assign))
        .route("/api/v1/admin/assignments/remove", post(unassign))
        .route("/api/v1/admin/users", get(list_users).post(create_user))
}

/// Reject a caller who is not an administrator.
///
/// Answers 404 rather than 403 for a non-administrator, so the administration
/// surface does not confirm its own existence to an ordinary account.
pub async fn require_admin(request: Request, next: Next) -> Result<Response, StatusCode> {
    let is_admin = request
        .extensions()
        .get::<SessionUser>()
        .is_some_and(|session| session.user.is_admin);
    if is_admin {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

fn failed(what: &str, error: anyhow::Error) -> Response {
    warn!(%error, "{what}");
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": error.to_string() })),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Providers
// ---------------------------------------------------------------------------

/// A provider and who may use it, which is what the administration surface
/// has to show together: a provider nobody is assigned to is configured and
/// unreachable, and that has to be visible at a glance.
#[derive(serde::Serialize)]
struct ProviderRow {
    #[serde(flatten)]
    provider: providers::ProviderSummary,
    assigned_users: Vec<String>,
    assigned_groups: Vec<String>,
}

async fn list_providers(State(state): State<Arc<AppState>>) -> Response {
    let summaries = match providers::list_providers(&state.store) {
        Ok(rows) => rows,
        Err(error) => return failed("listing providers failed", error),
    };

    let mut rows = Vec::with_capacity(summaries.len());
    for provider in summaries {
        let assignments = match providers::assignments_for_provider(&state.store, &provider.id) {
            Ok(rows) => rows,
            Err(error) => return failed("listing assignments failed", error),
        };
        let (mut assigned_users, mut assigned_groups) = (Vec::new(), Vec::new());
        for (kind, subject) in assignments {
            match kind {
                SubjectKind::User => assigned_users.push(subject),
                SubjectKind::Group => assigned_groups.push(subject),
            }
        }
        rows.push(ProviderRow {
            provider,
            assigned_users,
            assigned_groups,
        });
    }
    Json(rows).into_response()
}

async fn create_provider(
    State(state): State<Arc<AppState>>,
    Json(input): Json<ProviderInput>,
) -> Response {
    match providers::create_provider(&state.store, &state.sealer, &input) {
        Ok(id) => (StatusCode::CREATED, Json(serde_json::json!({ "id": id }))).into_response(),
        Err(error) => failed("creating the provider failed", error),
    }
}

async fn delete_provider(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match providers::delete_provider(&state.store, &id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => failed("deleting the provider failed", error),
    }
}

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct NameBody {
    name: String,
}

async fn list_groups(State(state): State<Arc<AppState>>) -> Response {
    match providers::list_groups(&state.store) {
        Ok(rows) => Json(rows).into_response(),
        Err(error) => failed("listing groups failed", error),
    }
}

async fn create_group(State(state): State<Arc<AppState>>, Json(body): Json<NameBody>) -> Response {
    match providers::create_group(&state.store, &body.name) {
        Ok(id) => (StatusCode::CREATED, Json(serde_json::json!({ "id": id }))).into_response(),
        Err(error) => failed("creating the group failed", error),
    }
}

async fn delete_group(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    match providers::delete_group(&state.store, &id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => failed("deleting the group failed", error),
    }
}

// ---------------------------------------------------------------------------
// Membership
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MembershipBody {
    user_id: String,
    group_id: String,
}

async fn add_membership(
    State(state): State<Arc<AppState>>,
    Json(body): Json<MembershipBody>,
) -> Response {
    match providers::add_membership(&state.store, &body.user_id, &body.group_id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => failed("adding the membership failed", error),
    }
}

async fn remove_membership(
    State(state): State<Arc<AppState>>,
    Json(body): Json<MembershipBody>,
) -> Response {
    match providers::remove_membership(&state.store, &body.user_id, &body.group_id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => failed("removing the membership failed", error),
    }
}

// ---------------------------------------------------------------------------
// Assignment
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AssignmentBody {
    provider_id: String,
    subject_kind: SubjectKind,
    subject_id: String,
}

async fn assign(State(state): State<Arc<AppState>>, Json(body): Json<AssignmentBody>) -> Response {
    // Answering 404 for an unknown provider rather than storing a row that
    // matches nothing, because such a row is silently useless.
    match providers::provider_exists(&state.store, &body.provider_id) {
        Ok(false) => return StatusCode::NOT_FOUND.into_response(),
        Err(error) => return failed("looking up the provider failed", error),
        Ok(true) => {}
    }
    match providers::assign(
        &state.store,
        &body.provider_id,
        body.subject_kind,
        &body.subject_id,
    ) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => failed("assigning the provider failed", error),
    }
}

async fn unassign(
    State(state): State<Arc<AppState>>,
    Json(body): Json<AssignmentBody>,
) -> Response {
    match providers::unassign(
        &state.store,
        &body.provider_id,
        body.subject_kind,
        &body.subject_id,
    ) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => failed("unassigning the provider failed", error),
    }
}

// ---------------------------------------------------------------------------
// Users
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct NewUserBody {
    email: String,
    password: String,
    #[serde(default)]
    is_admin: bool,
}

async fn list_users(State(state): State<Arc<AppState>>) -> Response {
    match accounts::list_users(&state.store) {
        Ok(rows) => Json(rows).into_response(),
        Err(error) => failed("listing users failed", error),
    }
}

async fn create_user(
    State(state): State<Arc<AppState>>,
    Json(body): Json<NewUserBody>,
) -> Response {
    match accounts::create_user(&state.store, &body.email, &body.password, body.is_admin) {
        Ok(id) => (StatusCode::CREATED, Json(serde_json::json!({ "id": id }))).into_response(),
        Err(error) => failed("creating the user failed", error),
    }
}
