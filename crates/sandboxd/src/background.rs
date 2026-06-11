//! Background loops: hot-pool warmer, idle auto-stop detector, and node
//! heartbeat (spec §9.1 hot pools, §13 auto-stop, §8 scaling triggers).

use crate::lifecycle::State;
use crate::service;
use crate::state::AppState;
use chrono::Utc;
use std::time::Duration;

/// Reconcile hot pools toward their targets (spec §9.1, §10.1).
pub fn spawn_warmer(state: AppState) {
    let interval = state.cfg.hotpool.warm_interval_seconds.max(1);
    tokio::spawn(async move {
        if !state.cfg.hotpool.enabled {
            return;
        }
        loop {
            let warmed = state.local.warm_once().await;
            if warmed > 0 {
                tracing::debug!(warmed, "warmed hot-pool VMs");
            }
            tokio::time::sleep(Duration::from_secs(interval)).await;
        }
    });
}

/// Auto-stop sandboxes idle past their window (spec §13, §14 auto-stop on).
/// Browser/VNC activity also bumps `last_active_at` via exec/preview touches.
pub fn spawn_idle_reaper(state: AppState) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let active = match state.store.all_active_sandboxes() {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "idle reaper: list failed");
                    continue;
                }
            };
            let now = Utc::now();
            for sb in active {
                if sb.state != State::Running {
                    continue;
                }
                let idle = (now - sb.last_active_at).num_seconds();
                if idle >= sb.auto_stop_seconds as i64 {
                    tracing::info!(sandbox = %sb.id, idle_s = idle, "auto-stopping idle sandbox");
                    if let Err(e) = service::stop_sandbox(&state, sb).await {
                        tracing::warn!(error = %e, "auto-stop failed");
                    }
                }
            }
        }
    });
}

/// Garbage-collect expired ephemeral images once no active sandbox references
/// them (feature). Soft-deletes so running sandboxes are unaffected (spec §25.3).
pub fn spawn_image_gc(state: AppState) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let now = Utc::now();
            let images = match state.store.all_images() {
                Ok(v) => v,
                Err(_) => continue,
            };
            let active = state.store.all_active_sandboxes().unwrap_or_default();
            for mut img in images {
                if !img.ephemeral {
                    continue;
                }
                let expired = img.expires_at.map(|t| now >= t).unwrap_or(false);
                if !expired {
                    continue;
                }
                let reference = img.reference();
                let referenced = active.iter().any(|s| s.image == reference || s.image == img.name);
                if referenced {
                    continue;
                }
                img.status = crate::images::ImageStatus::Deleted;
                img.updated_at = now;
                if state.store.put_image(&img).is_ok() {
                    tracing::info!(image = %reference, "GC'd expired ephemeral image");
                }
            }
        }
    });
}

/// Keep the local node's heartbeat fresh so the registry/dashboard see it live.
pub fn spawn_heartbeat(state: AppState) {
    tokio::spawn(async move {
        loop {
            if let Ok(Some(mut node)) = state.store.get_node(&state.local_node_id) {
                node.last_heartbeat_at = Utc::now();
                state.store.put_node(&node).ok();
            }
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    });
}
