//! JSON response builders, kept separate so handlers stay declarative and the
//! exact wire shapes (spec §19) live in one place.

use crate::catalog::{classify, ImageClass};
use crate::model::Sandbox;
use crate::pricing;
use crate::state::AppState;
use chrono::Utc;
use serde_json::{json, Value};

pub fn resources_view(sb: &Sandbox) -> Value {
    json!({
        "cpu": sb.resources.cpu_label(),
        "memory_mb": sb.resources.memory_mb,
        "disk_gb": sb.resources.disk_gb,
    })
}

fn urls_view(state: &AppState, sb: &Sandbox) -> Value {
    let mut ports = serde_json::Map::new();
    for &p in &sb.ports {
        ports.insert(p.to_string(), Value::String(state.preview_url(&sb.id, p)));
    }
    let mut urls = json!({ "ports": ports });
    if sb.browser_enabled() {
        urls["vnc"] = Value::String(state.preview_url(&sb.id, 6080));
        urls["cdp"] = Value::String(state.preview_url(&sb.id, 9222));
    }
    urls
}

/// Full sandbox view used by create and GET (spec §19 create response + §23
/// user dashboard fields).
pub fn sandbox_view(state: &AppState, sb: &Sandbox) -> Value {
    let class = classify(&sb.image).unwrap_or(ImageClass::Base);
    let quote = pricing::quote(&state.cfg.pricing, &sb.resources, &class);
    let uptime_seconds = (Utc::now() - sb.created_at).num_seconds().max(0);
    let cost_estimate_usd = if sb.state.is_active() {
        quote.price_usd_second * uptime_seconds as f64
    } else {
        0.0
    };

    let mut v = json!({
        "id": sb.id,
        "runtime": "firecracker",
        "image": sb.image,
        "state": sb.state.as_str(),
        "resources": resources_view(sb),
        "node_id": sb.node_id,
        "boot_path": sb.boot_path.as_str(),
        "boot_ms": sb.timings.boot_ms,
        "auto_stop_seconds": sb.auto_stop_seconds,
        "snapshot_enabled": sb.snapshot_enabled,
        "timings": sb.timings,
        "urls": urls_view(state, sb),
        "price": quote,
        "uptime_seconds": uptime_seconds,
        "cost_estimate_usd": (cost_estimate_usd * 1e6).round() / 1e6,
        "created_at": sb.created_at,
        "docker": sb.docker,
        "secret_names": sb.secret_names,
        "mounts": sb.mounts,
    });
    // The runtime label reflects the actual data plane in use.
    v["runtime"] = Value::String(state.local.runtime_kind().to_string());
    if sb.timings.browser_ready_ms > 0 {
        v["browser_ready_ms"] = json!(sb.timings.browser_ready_ms);
    }
    if let Some(err) = &sb.error {
        v["error"] = Value::String(err.clone());
    }
    v
}
