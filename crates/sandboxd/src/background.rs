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

/// Park sandboxes idle past their window in perpetual standby (roadmap Phase 1):
/// snapshot, free RAM, `$0` billing, auto-resume on the next request. This is
/// the loop that reframes workdir from a sandbox API into a perpetual-sandbox
/// platform — an idle sandbox stays logically alive but stops costing anything.
///
/// A sandbox with resident secrets is never snapshotted (review M3), so it
/// falls back to a plain stop (explicit resume required). Browser/VNC activity
/// bumps `last_active_at` via exec/preview touches, as before.
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
                if idle < sb.auto_stop_seconds as i64 {
                    continue;
                }
                if state.store.has_running_exec_jobs(&sb.id).unwrap_or(false) {
                    continue;
                }
                // Standby is gated (off by default) so the snapshot/restore path
                // is validated on a node before real sandboxes depend on it. A
                // sandbox with resident secrets is never snapshotted (review M3),
                // so it always takes the plain stop path.
                if state.cfg.standby.enabled && sb.secret_names.is_empty() {
                    tracing::info!(sandbox = %sb.id, idle_s = idle, "idle -> standby (snapshot + free RAM)");
                    if let Err(e) = service::standby_sandbox(&state, sb).await {
                        tracing::warn!(error = %e, "standby failed");
                    }
                } else {
                    tracing::info!(sandbox = %sb.id, idle_s = idle, "idle -> stop");
                    if let Err(e) = service::stop_sandbox(&state, sb).await {
                        tracing::warn!(error = %e, "auto-stop failed");
                    }
                }
            }
        }
    });
}

/// Soft standby (the tier between "running" and snapshot eviction): after
/// `standby.balloon_idle_seconds` of idleness, inflate the guest balloon so
/// free guest memory returns to the host — the sandbox stays hot (zero resume
/// latency) at a fraction of its RSS. Deflated again once activity returns
/// (checked each tick; `deflate_on_oom` already lets the guest reclaim pages
/// under its own pressure in the meantime). No-op unless `runtime.balloon` is
/// on and the window is non-zero.
pub fn spawn_balloon_reaper(state: AppState) {
    tokio::spawn(async move {
        let idle_s = state.cfg.standby.balloon_idle_seconds;
        if idle_s == 0 || !state.cfg.runtime.balloon {
            return;
        }
        // Sandboxes this loop has ballooned and not yet deflated.
        let mut ballooned: std::collections::HashSet<String> = Default::default();
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let active = match state.store.all_active_sandboxes() {
                Ok(v) => v,
                Err(_) => continue,
            };
            let now = Utc::now();
            for sb in &active {
                if sb.state != State::Running {
                    ballooned.remove(&sb.id);
                    continue;
                }
                let handle = match &sb.runtime_handle {
                    Some(h) => h.clone(),
                    None => continue,
                };
                let idle = (now - sb.last_active_at).num_seconds();
                let node = state.node_for(sb.node_id.as_deref().unwrap_or(""));
                if idle >= idle_s as i64 && !ballooned.contains(&sb.id) {
                    // Reclaim everything above a small working floor; the guest
                    // keeps what it actually needs (deflate_on_oom).
                    let target = sb.resources.memory_mb.saturating_sub(384);
                    match node.balloon(&handle, target).await {
                        Ok(()) => {
                            tracing::info!(sandbox = %sb.id, target_mib = target, idle_s = idle, "soft standby: balloon inflated");
                            ballooned.insert(sb.id.clone());
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, sandbox = %sb.id, "balloon inflate failed")
                        }
                    }
                } else if idle < idle_s as i64 && ballooned.contains(&sb.id) {
                    match node.balloon(&handle, 0).await {
                        Ok(()) => {
                            tracing::info!(sandbox = %sb.id, "activity returned: balloon deflated");
                            ballooned.remove(&sb.id);
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, sandbox = %sb.id, "balloon deflate failed")
                        }
                    }
                }
            }
            ballooned.retain(|id| active.iter().any(|s| &s.id == id));
        }
    });
}

/// Under measured memory pressure, park the least-recently-active running
/// sandbox in standby AHEAD of its idle window. This is the backpressure that
/// makes measured-memory overcommit (`[capacity] overcommit`) safe: admission
/// can run past the static shape-sum ceiling because pressure sheds the
/// longest-idle guests first, at $0 and transparently resumable. No-op unless
/// `[capacity] psi_standby_threshold > 0` and `[standby] enabled` (and PSI is
/// only available on Linux).
pub fn spawn_pressure_reaper(state: AppState) {
    tokio::spawn(async move {
        let threshold = state.cfg.capacity.psi_standby_threshold;
        if threshold <= 0.0 || !state.cfg.standby.enabled {
            return;
        }
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            let Some(avg10) = crate::capacity::memory_pressure_avg10() else {
                continue;
            };
            if avg10 < threshold {
                continue;
            }
            let active = match state.store.all_active_sandboxes() {
                Ok(v) => v,
                Err(_) => continue,
            };
            // One victim per tick: standby itself relieves pressure, so let the
            // next reading decide whether more shedding is needed.
            let Some(victim) = pick_pressure_victim(&state, &active) else {
                continue;
            };
            tracing::warn!(
                avg10,
                threshold,
                sandbox = %victim.id,
                "memory pressure — parking least-recently-active sandbox in standby"
            );
            if let Err(e) = service::standby_sandbox(&state, victim.clone()).await {
                tracing::warn!(error = %e, sandbox = %victim.id, "pressure standby failed");
            }
        }
    });
}

/// The least-recently-active RUNNING sandbox without resident secrets (secrets
/// are never snapshotted, so those fall through to the normal idle stop).
fn pick_pressure_victim<'a>(
    state: &AppState,
    active: &'a [crate::model::Sandbox],
) -> Option<&'a crate::model::Sandbox> {
    active
        .iter()
        .filter(|s| s.state == State::Running && s.secret_names.is_empty())
        .filter(|s| !state.store.has_running_exec_jobs(&s.id).unwrap_or(false))
        .min_by_key(|s| s.last_active_at)
}

/// Stop sandboxes for orgs whose real-time balance has hit zero. Persisted
/// `spent_usd` only updates when an interval closes, so without this a
/// long-running sandbox bills indefinitely past the org's prepaid credit
/// (review #8). The bootstrap admin org is exempt (mirrors the create bypass).
pub fn spawn_credit_enforcer(state: AppState) {
    tokio::spawn(async move {
        let admin_org = state.cfg.auth.bootstrap_org.clone();
        loop {
            tokio::time::sleep(Duration::from_secs(20)).await;
            let active = match state.store.all_active_sandboxes() {
                Ok(v) => v,
                Err(_) => continue,
            };
            let now = Utc::now();
            // Distinct orgs that currently have running sandboxes.
            let mut orgs: Vec<String> = active.iter().map(|s| s.org_id.clone()).collect();
            orgs.sort();
            orgs.dedup();
            for org_id in orgs {
                if org_id == admin_org {
                    continue;
                }
                let org = match state.store.get_org(&org_id) {
                    Ok(Some(o)) => o,
                    _ => continue,
                };
                let intervals = state.store.usage_for_org(&org_id).unwrap_or_default();
                if crate::usage::live_balance_usd(&org, &intervals, now) > 0.0 {
                    continue;
                }
                tracing::warn!(org = %org_id, "org out of credit — stopping its sandboxes");
                for sb in active
                    .iter()
                    .filter(|s| s.org_id == org_id && s.state == State::Running)
                {
                    if let Err(e) = service::stop_sandbox(&state, sb.clone()).await {
                        tracing::warn!(error = %e, sandbox = %sb.id, "credit-stop failed");
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
                let referenced = active
                    .iter()
                    .any(|s| s.image == reference || s.image == img.name);
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

/// Periodically reclaim stale per-VM jail/chroot directories left behind by
/// teardown under the jailer (disk-only, but they accumulate). Runs every 5
/// minutes and only removes directories not owned by a live VM and older than a
/// safety window (so a mid-boot VM is never touched).
pub fn spawn_jail_gc(state: AppState) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(300)).await;
            let removed = state.local.runtime().gc_stale_jails();
            if removed > 0 {
                tracing::info!(removed, "reclaimed stale jail directories");
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
