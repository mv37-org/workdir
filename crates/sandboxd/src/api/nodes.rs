//! Node endpoints (spec §8, §19, §23). Capacity is reported in
//! default-equivalent units (1 unit = 1 vCPU / 2 GB / 8 GB base sandbox).

use crate::auth::AuthContext;
use crate::capacity::{units_for_memory_gb, UNIT_MEMORY_GB};
use crate::error::{ApiError, ApiResult};
use crate::ids;
use crate::state::AppState;
use crate::store::active_memory_gb;
use axum::extract::{Path, State};
use axum::{Extension, Json};
use chrono::Utc;
use serde_json::{json, Value};

const JOIN_TOKEN_KEY: &str = "join_token";

pub async fn list(
    State(state): State<AppState>,
    Extension(_ctx): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    let nodes = state.store.list_nodes().map_err(ApiError::Internal)?;
    let mut views = vec![];
    let mut cluster_total_units = 0.0;
    let mut cluster_used_units = 0.0;
    for node in &nodes {
        let cap = node.capacity();
        let active = state.store.active_sandboxes_on_node(&node.id).map_err(ApiError::Internal)?;
        let used_units = units_for_memory_gb(active_memory_gb(&active));
        let free_units = (cap.practical_units as f64 - used_units).max(0.0);
        cluster_total_units += cap.practical_units as f64;
        cluster_used_units += used_units;
        let pools = if node.id == state.local_node_id {
            serde_json::to_value(state.local.pool_status().await).unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        views.push(json!({
            "id": node.id,
            "hostname": node.hostname,
            "role": node.role,
            "schedulable": node.schedulable,
            "draining": node.draining,
            "kvm_ok": node.kvm_ok,
            "total_memory_gb": node.total_memory_gb,
            "capacity": {
                "usable_for_sandboxes_gb": cap.usable_for_sandboxes_gb,
                "theoretical_units": cap.theoretical_units,
                "practical_units": cap.practical_units,
                "used_units": (used_units * 10.0).round() / 10.0,
                "free_units": (free_units * 10.0).round() / 10.0,
                "active_sandboxes": active.len(),
            },
            "hot_pools": pools,
            "registered_at": node.registered_at,
            "last_heartbeat_at": node.last_heartbeat_at,
        }));
    }

    let free_units = (cluster_total_units - cluster_used_units).max(0.0);
    let monthly_node_cost = state.cfg.pricing.monthly_node_cost_usd;
    let domain = &state.cfg.server.public_domain;
    Ok(Json(json!({
        "nodes": views,
        "cluster": {
            "total_units": cluster_total_units,
            "used_units": (cluster_used_units * 10.0).round() / 10.0,
            "free_units": (free_units * 10.0).round() / 10.0,
            "unit_definition": format!("1 unit = 1 vCPU / {} GB / 8 GB base sandbox", UNIT_MEMORY_GB),
        },
        "add_node": {
            "expected_monthly_node_cost_usd": monthly_node_cost,
            "projected_added_units": crate::capacity::node_capacity(64.0).practical_units,
            "install_command": format!(
                "curl -fsSL https://deploy.{domain}/install.sh | sudo bash -s -- \\\n  --role worker --control-plane https://api.{domain} --join-token <token>"
            ),
        }
    })))
}

pub async fn join_token(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    if !ctx.admin {
        return Err(ApiError::Forbidden("admin only".into()));
    }
    // Rotate to a fresh token (spec §18: rotatable control-plane secret).
    let token = ids::join_token();
    state.store.set_meta(JOIN_TOKEN_KEY, &token).map_err(ApiError::Internal)?;
    Ok(Json(json!({
        "join_token": token,
        "control_plane_url": format!("https://api.{}", state.cfg.server.public_domain),
        "note": "present this token on worker install; it is shown once here",
    })))
}

pub async fn drain(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    if !ctx.admin {
        return Err(ApiError::Forbidden("admin only".into()));
    }
    let mut node = state
        .store
        .get_node(&id)
        .map_err(ApiError::Internal)?
        .ok_or_else(|| ApiError::NotFound(format!("node {id}")))?;
    // Mark unschedulable; the drain playbook (§24.2) lets ephemeral sandboxes
    // finish or auto-stop before removal.
    node.schedulable = false;
    node.draining = true;
    node.last_heartbeat_at = Utc::now();
    state.store.put_node(&node).map_err(ApiError::Internal)?;
    let remaining = state.store.active_sandboxes_on_node(&id).map_err(ApiError::Internal)?;
    Ok(Json(json!({
        "id": id,
        "draining": true,
        "schedulable": false,
        "active_sandboxes_remaining": remaining.len(),
        "next": "stop assigning new sandboxes; let ephemeral sandboxes finish or auto-stop; export persistent snapshots; then remove the node",
    })))
}
