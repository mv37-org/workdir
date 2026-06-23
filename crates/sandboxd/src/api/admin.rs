//! Admin provisioning endpoints used by the Cloudflare control panel to create
//! orgs and register/revoke API keys that this daemon will accept.
//!
//! Both sides store only the SHA-256 hash of a key — the plaintext is generated
//! and shown once by the web app and never travels to the daemon.

use crate::auth::AuthContext;
use crate::error::{ApiError, ApiResult};
use crate::lifecycle::State as LfState;
use crate::state::AppState;
use crate::usage::{ApiKey, Org, OrgStatus};
use crate::{secrets, service, views};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};

fn require_admin(ctx: &AuthContext) -> ApiResult<()> {
    if ctx.admin {
        Ok(())
    } else {
        Err(ApiError::Forbidden("admin only".into()))
    }
}

#[derive(Deserialize)]
pub struct CreateOrgReq {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub prepaid_credits_usd: Option<f64>,
    #[serde(default)]
    pub quota_units: Option<f64>,
}

/// Create or update an org. Idempotent on `id` so the web app can call it
/// before every key issuance without checking existence first.
pub async fn create_org(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(req): Json<CreateOrgReq>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    require_admin(&ctx)?;
    if req.id.is_empty() {
        return Err(ApiError::BadRequest("org id required".into()));
    }
    let existing = state.store.get_org(&req.id).map_err(ApiError::Internal)?;
    let org = match existing {
        Some(mut o) => {
            // Update mutable fields, keep accrued spend/status.
            o.name = req.name;
            if let Some(c) = req.prepaid_credits_usd {
                o.prepaid_credits_usd = c;
            }
            if let Some(q) = req.quota_units {
                o.quota_units = q;
            }
            o
        }
        None => Org {
            id: req.id.clone(),
            name: req.name,
            status: OrgStatus::Active,
            prepaid_credits_usd: req.prepaid_credits_usd.unwrap_or(5.0), // free starter credit
            spent_usd: 0.0,
            quota_units: req.quota_units.unwrap_or(0.0), // 0 = unlimited
            created_at: Utc::now(),
        },
    };
    state.store.put_org(&org).map_err(ApiError::Internal)?;
    Ok((
        StatusCode::OK,
        Json(json!({ "org_id": org.id, "status": org.status })),
    ))
}

#[derive(Deserialize)]
pub struct RegisterKeyReq {
    pub org_id: String,
    /// SHA-256 hex of the full `sk_live_...` key (computed by the web app).
    pub key_hash: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// Register a customer API key by its hash so the daemon accepts it.
pub async fn register_key(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(req): Json<RegisterKeyReq>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    require_admin(&ctx)?;
    if state
        .store
        .get_org(&req.org_id)
        .map_err(ApiError::Internal)?
        .is_none()
    {
        return Err(ApiError::BadRequest(format!(
            "org '{}' does not exist",
            req.org_id
        )));
    }
    if req.key_hash.len() != 64 || !req.key_hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ApiError::BadRequest(
            "key_hash must be a SHA-256 hex digest".into(),
        ));
    }
    let key = ApiKey {
        key_hash: req.key_hash.clone(),
        org_id: req.org_id,
        name: req.name.unwrap_or_else(|| "dashboard".into()),
        admin: false,
        disabled: false,
        created_at: Utc::now(),
    };
    state.store.put_api_key(&key).map_err(ApiError::Internal)?;
    Ok((StatusCode::CREATED, Json(json!({ "registered": true }))))
}

/// Disable a key (revoke). Kept (not deleted) so it stays auditable.
pub async fn revoke_key(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(hash): Path<String>,
) -> ApiResult<Json<Value>> {
    require_admin(&ctx)?;
    let mut key = state
        .store
        .get_api_key(&hash)
        .map_err(ApiError::Internal)?
        .ok_or_else(|| ApiError::NotFound("key".into()))?;
    key.disabled = true;
    state.store.put_api_key(&key).map_err(ApiError::Internal)?;
    Ok(Json(json!({ "revoked": true })))
}

// --- per-org management on behalf of the control panel ----------------------
// The control panel holds the admin key and acts for a logged-in user's org.
// All of these verify the resource belongs to `org` so one org's dashboard can
// never touch another's.

/// List an org's secret names + timestamps (never values).
pub async fn org_secrets_list(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(org): Path<String>,
) -> ApiResult<Json<Value>> {
    require_admin(&ctx)?;
    let recs = state.store.list_secrets(&org).map_err(ApiError::Internal)?;
    let names: Vec<Value> = recs
        .iter()
        .map(|r| json!({ "name": r.name, "created_at": r.created_at, "updated_at": r.updated_at }))
        .collect();
    Ok(Json(json!({ "secrets": names })))
}

/// Set (create/overwrite) an org secret. Write-only — the value never reads back.
pub async fn org_secret_put(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path((org, name)): Path<(String, String)>,
    Json(body): Json<crate::api::secrets::PutSecretBody>,
) -> ApiResult<Json<Value>> {
    require_admin(&ctx)?;
    if state
        .store
        .get_org(&org)
        .map_err(ApiError::Internal)?
        .is_none()
    {
        return Err(ApiError::NotFound(format!("org {org}")));
    }
    if !secrets::valid_name(&name) {
        return Err(ApiError::BadRequest(
            "secret name must be an env-style identifier (letters, digits, underscore; not starting with a digit)".into(),
        ));
    }
    if body.value.len() > 64 * 1024 {
        return Err(ApiError::BadRequest(
            "secret value too large (max 64 KiB)".into(),
        ));
    }
    let rec = secrets::encrypt(&state.secret_key, &org, &name, &body.value)
        .map_err(ApiError::Internal)?;
    state.store.put_secret(&rec).map_err(ApiError::Internal)?;
    Ok(Json(json!({ "name": name, "stored": true })))
}

pub async fn org_secret_delete(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path((org, name)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    require_admin(&ctx)?;
    let removed = state
        .store
        .delete_secret(&org, &name)
        .map_err(ApiError::Internal)?;
    if !removed {
        return Err(ApiError::NotFound(format!("secret {name}")));
    }
    Ok(Json(json!({ "name": name, "deleted": true })))
}

/// List an org's sandboxes (excludes deleted), newest first.
pub async fn org_sandboxes(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(org): Path<String>,
) -> ApiResult<Json<Value>> {
    require_admin(&ctx)?;
    let mut sbs = state
        .store
        .list_sandboxes_for_org(&org)
        .map_err(ApiError::Internal)?;
    sbs.retain(|s| s.state != LfState::Deleted);
    sbs.sort_by_key(|sb| std::cmp::Reverse(sb.created_at));
    let views: Vec<Value> = sbs
        .iter()
        .map(|sb| views::sandbox_view(&state, sb))
        .collect();
    Ok(Json(json!({ "sandboxes": views })))
}

fn owned_sandbox(state: &AppState, org: &str, id: &str) -> ApiResult<crate::model::Sandbox> {
    let sb = state
        .store
        .get_sandbox(id)
        .map_err(ApiError::Internal)?
        .filter(|s| s.org_id == org)
        .ok_or_else(|| ApiError::NotFound(format!("sandbox {id}")))?;
    Ok(sb)
}

pub async fn org_sandbox_stop(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path((org, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    require_admin(&ctx)?;
    let sb = owned_sandbox(&state, &org, &id)?;
    let updated = service::stop_sandbox(&state, sb).await?;
    Ok(Json(views::sandbox_view(&state, &updated)))
}

pub async fn org_sandbox_delete(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path((org, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    require_admin(&ctx)?;
    let sb = owned_sandbox(&state, &org, &id)?;
    service::delete_sandbox(&state, sb).await?;
    Ok(Json(json!({ "id": id, "deleted": true })))
}

/// Kill switch: suspend an org (block new creates + stop running sandboxes).
pub async fn org_suspend(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(org): Path<String>,
) -> ApiResult<Json<Value>> {
    require_admin(&ctx)?;
    if org == state.cfg.auth.bootstrap_org {
        return Err(ApiError::BadRequest(
            "refusing to suspend the bootstrap admin org".into(),
        ));
    }
    let mut o = state
        .store
        .get_org(&org)
        .map_err(ApiError::Internal)?
        .ok_or_else(|| ApiError::NotFound(format!("org {org}")))?;
    o.status = OrgStatus::Suspended;
    state.store.put_org(&o).map_err(ApiError::Internal)?;
    // Stop everything the org has running right now.
    let sbs = state
        .store
        .list_sandboxes_for_org(&org)
        .map_err(ApiError::Internal)?;
    let mut stopped = 0;
    for sb in sbs.into_iter().filter(|s| s.state == LfState::Running) {
        if service::stop_sandbox(&state, sb).await.is_ok() {
            stopped += 1;
        }
    }
    tracing::warn!(org = %org, stopped, "org suspended (kill switch)");
    Ok(Json(
        json!({ "org": org, "status": "suspended", "stopped": stopped }),
    ))
}

pub async fn org_unsuspend(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(org): Path<String>,
) -> ApiResult<Json<Value>> {
    require_admin(&ctx)?;
    let mut o = state
        .store
        .get_org(&org)
        .map_err(ApiError::Internal)?
        .ok_or_else(|| ApiError::NotFound(format!("org {org}")))?;
    o.status = OrgStatus::Active;
    state.store.put_org(&o).map_err(ApiError::Internal)?;
    Ok(Json(json!({ "org": org, "status": "active" })))
}

/// List an org's custom images (build status, version, log, timestamps).
pub async fn org_images(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(org): Path<String>,
) -> ApiResult<Json<Value>> {
    require_admin(&ctx)?;
    let imgs = state
        .store
        .list_images_for_org(&org)
        .map_err(ApiError::Internal)?;
    let views: Vec<Value> = imgs.iter().map(crate::api::images::image_view).collect();
    Ok(Json(json!({ "images": views })))
}

/// Soft-delete a custom image (blocks new creates; running sandboxes unaffected).
pub async fn org_image_delete(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path((org, id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    require_admin(&ctx)?;
    let mut img = state
        .store
        .get_image(&id)
        .map_err(ApiError::Internal)?
        .filter(|i| i.org_id == org)
        .ok_or_else(|| ApiError::NotFound(format!("image {id}")))?;
    img.status = crate::images::ImageStatus::Deleted;
    img.updated_at = Utc::now();
    state.store.put_image(&img).map_err(ApiError::Internal)?;
    Ok(Json(json!({ "id": id, "deleted": true })))
}

/// Real-time operational metrics: host health + every live sandbox with its
/// shape, owner (org), and cost. Powers the operator monitoring dashboard.
pub async fn metrics(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    require_admin(&ctx)?;
    let now = Utc::now();

    let host = crate::host::collect(&state.cfg.server.data_dir).await;

    // Local node capacity.
    let node = state
        .store
        .get_node(&state.local_node_id)
        .map_err(ApiError::Internal)?;
    let active = state
        .store
        .all_active_sandboxes()
        .map_err(ApiError::Internal)?;

    let mut by_state: std::collections::BTreeMap<String, u64> = Default::default();
    let mut by_image: std::collections::BTreeMap<String, u64> = Default::default();
    let mut committed_memory_mb: u64 = 0;
    let mut sandboxes = Vec::with_capacity(active.len());
    for sb in &active {
        let class = crate::catalog::classify(&sb.image).unwrap_or(crate::catalog::ImageClass::Base);
        let price = crate::pricing::quote(&state.cfg.pricing, &sb.resources, &class).price_usd_hr;
        let uptime = (now - sb.created_at).num_seconds().max(0);
        committed_memory_mb += sb.resources.memory_mb as u64;
        *by_state.entry(sb.state.as_str().to_string()).or_default() += 1;
        *by_image.entry(sb.image.clone()).or_default() += 1;
        let net = sb
            .runtime_handle
            .as_ref()
            .and_then(|h| state.local.runtime().vm_net_stats(h));
        sandboxes.push(json!({
            "id": sb.id,
            "org_id": sb.org_id,
            "image": sb.image,
            "cpu": sb.resources.cpu,
            "memory_mb": sb.resources.memory_mb,
            "disk_gb": sb.resources.disk_gb,
            "state": sb.state.as_str(),
            "boot_path": sb.boot_path.as_str(),
            "created_at": sb.created_at,
            "uptime_seconds": uptime,
            "docker": sb.docker,
            "browser": sb.browser_enabled(),
            "secret_count": sb.secret_names.len(),
            "node_id": sb.node_id,
            "price_usd_hr": price,
            "tx_bytes": net.as_ref().map(|n| n.tx_bytes),
            "rx_bytes": net.as_ref().map(|n| n.rx_bytes),
        }));
    }

    let committed_units = committed_memory_mb as f64 / 1024.0 / crate::capacity::UNIT_MEMORY_GB;
    let capacity = node.as_ref().map(|n| {
        let cap = n.capacity();
        json!({
            "practical_units": cap.practical_units,
            "theoretical_units": cap.theoretical_units,
            "committed_units": (committed_units * 10.0).round() / 10.0,
            "free_units": (cap.practical_units as f64 - committed_units).max(0.0),
        })
    });
    let pools = state.local.pool_status().await;

    Ok(Json(json!({
        "at": now,
        "node": {
            "node_id": state.local_node_id,
            "host": host,
            "capacity": capacity,
            "runtime": state.local.runtime_kind(),
        },
        "hot_pools": pools,
        "summary": {
            "live_sandboxes": active.len(),
            "by_state": by_state,
            "by_image": by_image,
            "committed_memory_mb": committed_memory_mb,
            "committed_units": (committed_units * 10.0).round() / 10.0,
        },
        "sandboxes": sandboxes,
    })))
}

/// Per-org usage for the dashboard (admin view of any org).
pub async fn org_usage(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(org): Path<String>,
) -> ApiResult<Json<Value>> {
    require_admin(&ctx)?;
    let now = Utc::now();
    let intervals = state
        .store
        .usage_for_org(&org)
        .map_err(ApiError::Internal)?;
    let total_cost: f64 = intervals.iter().map(|iv| iv.cost_usd(now)).sum();
    let delivered: f64 = intervals
        .iter()
        .map(|iv| iv.delivered_unit_seconds(now))
        .sum();
    let sandboxes = state
        .store
        .list_sandboxes_for_org(&org)
        .map_err(ApiError::Internal)?;
    let active = sandboxes.iter().filter(|s| s.state.is_active()).count();
    let org_rec = state.store.get_org(&org).map_err(ApiError::Internal)?;
    Ok(Json(json!({
        "org_id": org,
        "total_cost_usd": (total_cost * 1e6).round() / 1e6,
        "delivered_unit_seconds": delivered.round(),
        "active_sandboxes": active,
        "total_sandboxes": sandboxes.len(),
        "balance_usd": org_rec.as_ref().map(|o| (o.balance_usd() * 1e6).round() / 1e6),
        "prepaid_credits_usd": org_rec.as_ref().map(|o| o.prepaid_credits_usd),
    })))
}
