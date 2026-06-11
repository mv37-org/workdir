//! Secret management endpoints (feature). Values are encrypted at rest and are
//! never returned over the API — only injected into sandboxes after assignment.

use crate::auth::AuthContext;
use crate::error::{ApiError, ApiResult};
use crate::secrets;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};

pub async fn list(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    let recs = state.store.list_secrets(&ctx.org_id).map_err(ApiError::Internal)?;
    let names: Vec<Value> = recs
        .iter()
        .map(|r| json!({ "name": r.name, "created_at": r.created_at, "updated_at": r.updated_at }))
        .collect();
    Ok(Json(json!({ "secrets": names })))
}

#[derive(Deserialize)]
pub struct PutSecretBody {
    pub value: String,
}

pub async fn put(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(name): Path<String>,
    Json(body): Json<PutSecretBody>,
) -> ApiResult<Json<Value>> {
    if !secrets::valid_name(&name) {
        return Err(ApiError::BadRequest(
            "secret name must be an env-style identifier (letters, digits, underscore; not starting with a digit)".into(),
        ));
    }
    if body.value.len() > 64 * 1024 {
        return Err(ApiError::BadRequest("secret value too large (max 64 KiB)".into()));
    }
    let rec = secrets::encrypt(&state.secret_key, &ctx.org_id, &name, &body.value)
        .map_err(ApiError::Internal)?;
    state.store.put_secret(&rec).map_err(ApiError::Internal)?;
    Ok(Json(json!({ "name": name, "stored": true })))
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(name): Path<String>,
) -> ApiResult<Json<Value>> {
    let removed = state.store.delete_secret(&ctx.org_id, &name).map_err(ApiError::Internal)?;
    if !removed {
        return Err(ApiError::NotFound(format!("secret {name}")));
    }
    Ok(Json(json!({ "name": name, "deleted": true })))
}
