//! Persistent volume endpoints (Phase 5). A volume is org-scoped block storage
//! backed by an ext4 image under `runtime.volumes_dir`. It can be attached to at
//! most one running sandbox at a time (`POST /v1/sandboxes` with `volumes`) and
//! survives that sandbox's deletion, so workspace state persists across sessions.

use crate::auth::AuthContext;
use crate::error::{ApiError, ApiResult};
use crate::ids;
use crate::model::{CreateVolumeRequest, Volume};
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::{Extension, Json};
use chrono::Utc;
use serde_json::{json, Value};

/// Constrained sizes, like the resource knobs — predictable packing on disk.
const ALLOWED_VOLUME_GB: &[u32] = &[1, 5, 10, 20, 50, 100, 250];

fn view(v: &Volume) -> Value {
    json!({
        "id": v.id,
        "name": v.name,
        "size_gb": v.size_gb,
        "attached_to": v.attached_to,
        "created_at": v.created_at,
        "updated_at": v.updated_at,
    })
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

pub async fn list(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    let vols = state.store.list_volumes_for_org(&ctx.org_id).map_err(ApiError::Internal)?;
    Ok(Json(json!({ "volumes": vols.iter().map(view).collect::<Vec<_>>() })))
}

pub async fn get(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let v = load_owned(&state, &ctx, &id)?;
    Ok(Json(view(&v)))
}

pub async fn create(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(req): Json<CreateVolumeRequest>,
) -> ApiResult<(axum::http::StatusCode, Json<Value>)> {
    if !valid_name(&req.name) {
        return Err(ApiError::BadRequest(
            "volume name must be 1–64 chars of letters, digits, '-' or '_'".into(),
        ));
    }
    if !ALLOWED_VOLUME_GB.contains(&req.size_gb) {
        return Err(ApiError::BadRequest(format!(
            "size_gb={} is not allowed; choose one of {ALLOWED_VOLUME_GB:?} GB",
            req.size_gb
        )));
    }
    if state
        .store
        .get_volume_by_name(&ctx.org_id, &req.name)
        .map_err(ApiError::Internal)?
        .is_some()
    {
        return Err(ApiError::Conflict(format!("a volume named '{}' already exists", req.name)));
    }

    let id = ids::volume_id();
    // Allocate the backing image: a sparse file, formatted ext4 with a stable
    // label so the guest can mount it by LABEL regardless of /dev/vdX ordering.
    let dir = &state.cfg.runtime.volumes_dir;
    std::fs::create_dir_all(dir).map_err(|e| ApiError::Internal(e.into()))?;
    let path = dir.join(format!("{id}.ext4"));
    let bytes = req.size_gb as u64 * 1024 * 1024 * 1024;
    allocate_ext4(&path, bytes, &ids::volume_label(&id))
        .await
        .map_err(ApiError::Internal)?;

    let now = Utc::now();
    let v = Volume {
        id: id.clone(),
        org_id: ctx.org_id.clone(),
        name: req.name,
        size_gb: req.size_gb,
        attached_to: None,
        created_at: now,
        updated_at: now,
    };
    state.store.put_volume(&v).map_err(ApiError::Internal)?;
    Ok((axum::http::StatusCode::CREATED, Json(view(&v))))
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let v = load_owned(&state, &ctx, &id)?;
    if let Some(sb) = &v.attached_to {
        return Err(ApiError::Conflict(format!(
            "volume is attached to sandbox {sb}; delete or stop that sandbox first"
        )));
    }
    let path = state.cfg.runtime.volumes_dir.join(format!("{id}.ext4"));
    let _ = std::fs::remove_file(&path);
    state.store.delete_volume(&id).map_err(ApiError::Internal)?;
    Ok(Json(json!({ "id": id, "deleted": true })))
}

/// Load a volume and enforce org ownership (404 otherwise — never leak existence
/// across orgs).
fn load_owned(state: &AppState, ctx: &AuthContext, id: &str) -> ApiResult<Volume> {
    let v = state.store.get_volume(id).map_err(ApiError::Internal)?;
    match v {
        Some(v) if v.org_id == ctx.org_id || ctx.admin => Ok(v),
        _ => Err(ApiError::NotFound(format!("volume {id}"))),
    }
}

/// Create a sparse `bytes`-sized ext4 image at `path`, labelled `label`.
async fn allocate_ext4(path: &std::path::Path, bytes: u64, label: &str) -> anyhow::Result<()> {
    use anyhow::{bail, Context};
    let f = std::fs::File::create(path).context("create volume image")?;
    f.set_len(bytes).context("size volume image")?;
    drop(f);
    let out = tokio::process::Command::new("mkfs.ext4")
        .args(["-F", "-q", "-L", label])
        .arg(path)
        .output()
        .await
        .context("run mkfs.ext4 (is e2fsprogs installed?)")?;
    if !out.status.success() {
        let _ = std::fs::remove_file(path);
        bail!("mkfs.ext4 failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}
