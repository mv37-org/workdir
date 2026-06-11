//! Usage, billing, admin overview, and public benchmark endpoints
//! (spec §21, §22, §23).

use crate::auth::AuthContext;
use crate::catalog::ImageClass;
use crate::error::{ApiError, ApiResult};
use crate::knobs::Resources;
use crate::pricing;
use crate::state::AppState;
use axum::extract::State;
use axum::{Extension, Json};
use chrono::{Datelike, TimeZone, Utc};
use serde_json::{json, Value};

pub async fn usage(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    let now = Utc::now();
    let intervals = state.store.usage_for_org(&ctx.org_id).map_err(ApiError::Internal)?;
    let total_cost: f64 = intervals.iter().map(|iv| iv.cost_usd(now)).sum();
    let delivered_unit_seconds: f64 = intervals.iter().map(|iv| iv.delivered_unit_seconds(now)).sum();
    let org = state.store.get_org(&ctx.org_id).map_err(ApiError::Internal)?;

    let mut per_sandbox = std::collections::BTreeMap::<String, (f64, f64)>::new();
    for iv in &intervals {
        let e = per_sandbox.entry(iv.sandbox_id.clone()).or_insert((0.0, 0.0));
        e.0 += iv.seconds(now);
        e.1 += iv.cost_usd(now);
    }
    let sandboxes: Vec<Value> = per_sandbox
        .into_iter()
        .map(|(id, (secs, cost))| json!({ "sandbox_id": id, "running_seconds": secs.round(), "cost_usd": round6(cost) }))
        .collect();

    Ok(Json(json!({
        "org_id": ctx.org_id,
        "total_cost_usd": round6(total_cost),
        "delivered_unit_seconds": delivered_unit_seconds.round(),
        "prepaid_credits_usd": org.as_ref().map(|o| o.prepaid_credits_usd),
        "balance_usd": org.as_ref().map(|o| round6(o.balance_usd())),
        "quota_units": org.as_ref().map(|o| o.quota_units),
        "sandboxes": sandboxes,
    })))
}

pub async fn admin_overview(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    if !ctx.admin {
        return Err(ApiError::Forbidden("admin only".into()));
    }
    let now = Utc::now();
    let nodes = state.store.list_nodes().map_err(ApiError::Internal)?;
    let all_usage = state.store.all_usage().map_err(ApiError::Internal)?;
    let active = state.store.all_active_sandboxes().map_err(ApiError::Internal)?;

    // Reconcile a MONTH of node cost against a MONTH of delivered units (review
    // #11): clip each interval to the current calendar month so the published
    // at-cost price doesn't trend to zero as history accumulates.
    let month_start = Utc
        .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
        .single()
        .unwrap_or(now);
    let delivered_unit_hours: f64 = all_usage
        .iter()
        .map(|iv| {
            let start = iv.started_at.max(month_start);
            let end = iv.ended_at.unwrap_or(now).max(start);
            let secs = (end - start).num_milliseconds().max(0) as f64 / 1000.0;
            secs * iv.resource_units * iv.image_multiplier
        })
        .sum::<f64>()
        / 3600.0;
    let monthly_node_cost = state.cfg.pricing.monthly_node_cost_usd * nodes.len().max(1) as f64;
    let platform_overhead = monthly_node_cost * 0.25; // control plane + fees + abuse reserve
    let reconciled = pricing::reconciled_unit_price_usd_hr(
        monthly_node_cost,
        platform_overhead,
        delivered_unit_hours.max(1e-6),
    );

    let base_price = pricing::sandbox_price_usd_hr(&state.cfg.pricing, &Resources::default(), &ImageClass::Base);

    Ok(Json(json!({
        "nodes": nodes.len(),
        "active_sandboxes": active.len(),
        "hot_pools": state.local.pool_status().await,
        "delivered_unit_hours": round6(delivered_unit_hours),
        "cost": {
            "monthly_node_cost_usd": monthly_node_cost,
            "platform_overhead_usd": platform_overhead,
            "reconciled_unit_price_usd_hr": round6(reconciled),
            "configured_unit_price_usd_hr": state.cfg.pricing.default_unit_price_usd_hr,
            "default_base_price_usd_hr": round6(base_price),
        },
        "abuse_alerts": [],
        "runtime": state.local.runtime_kind(),
    })))
}

pub async fn benchmarks(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    // Aggregate honest boot timings by image + boot path (spec §21.3). Hot-pool
    // numbers are labeled; cold/snapshot are reported separately, never hidden.
    let sandboxes = if ctx.admin {
        state.store.all_active_sandboxes().map_err(ApiError::Internal)?
    } else {
        state.store.list_sandboxes_for_org(&ctx.org_id).map_err(ApiError::Internal)?
    };

    let mut buckets: std::collections::BTreeMap<(String, String), Vec<u64>> = Default::default();
    for s in &sandboxes {
        let total = s.timings.boot_ms + s.timings.image_cache_ms;
        buckets
            .entry((s.image.clone(), s.boot_path.as_str().to_string()))
            .or_default()
            .push(total.max(1));
    }
    let mut series = vec![];
    for ((image, boot_path), mut vals) in buckets {
        vals.sort_unstable();
        series.push(json!({
            "image": image,
            "boot_path": boot_path,
            "samples": vals.len(),
            "create_to_echo_ms_p50": percentile(&vals, 50.0),
            "create_to_echo_ms_p95": percentile(&vals, 95.0),
        }));
    }

    let base_price = pricing::sandbox_price_usd_hr(&state.cfg.pricing, &Resources::default(), &ImageClass::Base);
    let nodes = state.store.list_nodes().map_err(ApiError::Internal)?;
    Ok(Json(json!({
        "series": series,
        "current_hosted_at_cost_default_price_usd_hr": round6(base_price),
        "node_count": nodes.len(),
        "note": "boot timings include image cache cost; hot_pool, snapshot_restore and cold_boot are reported separately and never merged",
    })))
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p / 100.0 * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

fn round6(v: f64) -> f64 {
    (v * 1_000_000.0).round() / 1_000_000.0
}
