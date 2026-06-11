//! Sandbox service: the create flow (spec §13.2), startup-recipe runner, and
//! lifecycle operations (stop/resume/delete), plus node-snapshot gathering for
//! the scheduler and usage accounting. Handlers stay thin; the policy lives
//! here.

use crate::auth::AuthContext;
use crate::capacity::units_for_memory_gb;
use crate::catalog::{classify, ImageClass};
use crate::error::{ApiError, ApiResult};
use crate::ids;
use crate::knobs::{validate_auto_stop, Resources};
use crate::lifecycle::State;
use crate::model::*;
use crate::node::NodeClient;
use crate::pricing;
use crate::runtime::{ExecRequest, VmSpec};
use crate::scheduler::{self, NodeSnapshot, PlacementRequest};
use crate::state::AppState;
use crate::store::{CasOutcome, Snapshot};
use crate::usage::{OrgStatus, UsageInterval};
use chrono::Utc;
use std::collections::BTreeMap;
use std::time::Instant;

/// Validate, place, boot, and run the startup recipe for a new sandbox.
pub async fn create_sandbox(
    state: &AppState,
    ctx: &AuthContext,
    req: CreateSandboxRequest,
) -> ApiResult<Sandbox> {
    let wall_start = Instant::now();

    // --- org / quota / credit admission (spec §22) ----------------------
    let org = state
        .store
        .get_org(&ctx.org_id)
        .map_err(ApiError::Internal)?
        .ok_or(ApiError::Unauthorized)?;
    if org.status == OrgStatus::Suspended {
        return Err(ApiError::Forbidden("org suspended".into()));
    }
    if !ctx.admin && org.balance_usd() <= 0.0 {
        return Err(ApiError::Forbidden("no prepaid credits remaining".into()));
    }

    // --- image classification (spec §10, §11) ---------------------------
    let image_ref = req.image.clone().unwrap_or_else(|| "base".to_string());
    let class = classify(&image_ref).map_err(ApiError::BadRequest)?;
    if let ImageClass::Custom(_) = &class {
        // Custom images must be built and published first; never build on create.
        let reference = match &req.image_version {
            Some(v) => format!("{image_ref}:{v}"),
            None => image_ref.clone(),
        };
        let found = state
            .store
            .find_ready_image(&reference)
            .map_err(ApiError::Internal)?;
        if found.is_none() {
            return Err(ApiError::BadRequest(format!(
                "custom image '{reference}' is not published; create and wait for it via POST /v1/images"
            )));
        }
    }

    // --- resource knobs (spec §3.2) -------------------------------------
    let resources = Resources::validate(&req.resources.clone().unwrap_or_default())
        .map_err(ApiError::BadRequest)?;
    enforce_minimums(&class, &resources)?;
    let auto_stop_seconds = validate_auto_stop(req.auto_stop_seconds).map_err(ApiError::BadRequest)?;

    // --- browser config (spec §12) --------------------------------------
    let browser = req.browser.clone().filter(|b| b.enabled);
    if browser.is_some() && !class.is_browser() {
        return Err(ApiError::BadRequest(
            "browser requires the 'browser' curated image".into(),
        ));
    }

    // --- features: secrets, files, mounts, docker -----------------------
    let startup = parse_startup(req.startup.clone())?;
    let (secret_env, secret_names) = resolve_secrets(state, ctx, startup.as_ref())?;
    let files = build_ephemeral_files(req.files.as_deref())?;
    let mounts = req.mounts.clone().unwrap_or_default();
    validate_mounts(&mounts)?;
    let docker = req.docker.as_ref().map(|d| d.enabled).unwrap_or(false);
    if docker && matches!(class, ImageClass::Base) {
        // The base image deliberately excludes the Docker daemon (spec §10.2).
        return Err(ApiError::BadRequest(
            "docker-in-docker requires a docker-capable image (heavy-build or a custom image); the base image has no docker daemon".into(),
        ));
    }

    // --- admission section (serialized; review #1) ----------------------
    // Hold the admission lock across quota + capacity check and the reservation
    // of the `creating` row, so concurrent creates cannot both pass a stale
    // snapshot and overcommit. Released before the (slow) VM boot.
    let admission_guard = state.admission.lock().await;

    // --- per-org quota in default-equivalent units ----------------------
    if !ctx.admin && !org.quota_unlimited() {
        let used = org_active_units(state, &ctx.org_id)?;
        let req_units = units_for_memory_gb(resources.memory_gb());
        if used + req_units > org.quota_units + 1e-9 {
            return Err(ApiError::Forbidden(format!(
                "org quota exceeded: {used:.1} + {req_units:.1} > {:.1} units",
                org.quota_units
            )));
        }
    }

    // --- placement (spec §15) -------------------------------------------
    let placement_req = PlacementRequest {
        org_id: ctx.org_id.clone(),
        image_key: class.key().to_string(),
        resources,
        browser_required: browser.is_some(),
        is_custom_image: class.is_custom(),
    };
    let snapshots = gather_node_snapshots(state, &placement_req).await?;
    let placement = scheduler::select(&placement_req, &snapshots)
        .map_err(|r| ApiError::NoCapacity { reason: r.reason, detail: r.detail })?;
    if placement.node_id != state.local_node_id {
        // Single control-plane build executes the data path locally. Remote
        // worker execution is the documented multi-node extension.
        return Err(ApiError::NoCapacity {
            reason: "remote_placement_unsupported".into(),
            detail: format!(
                "scheduler chose node {} but worker data-plane RPC is not enabled in this build",
                placement.node_id
            ),
        });
    }

    // --- build the runtime spec -----------------------------------------
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    if let Some(s) = &startup {
        env.extend(s.env.clone());
    }
    let sandbox_id = ids::sandbox_id();
    let spec = VmSpec {
        sandbox_id: sandbox_id.clone(),
        org_id: ctx.org_id.clone(),
        image_key: class.key().to_string(),
        image_ref: image_ref.clone(),
        resources,
        env,
        secret_env,
        browser: browser.clone(),
        docker,
        mounts: mounts.clone(),
        files,
    };

    // --- persist a creating record, then boot ---------------------------
    let now = Utc::now();
    let mut sb = Sandbox {
        id: sandbox_id.clone(),
        org_id: ctx.org_id.clone(),
        state: State::Creating,
        node_id: Some(placement.node_id.clone()),
        image: image_ref.clone(),
        resources,
        auto_stop_seconds,
        snapshot_enabled: req.snapshot.unwrap_or(false),
        browser: browser.clone(),
        startup: startup.clone(),
        boot_path: placement.boot_path_hint,
        timings: Timings::default(),
        secret_names,
        docker,
        mounts,
        ports: vec![],
        runtime_handle: None,
        error: None,
        created_at: now,
        updated_at: now,
        last_active_at: now,
    };
    state.store.put_sandbox(&sb).map_err(ApiError::Internal)?;
    // Capacity is now reserved (the `creating` row counts against admission);
    // release the lock so other creates proceed concurrently with this boot.
    drop(admission_guard);

    // Snapshot availability is per-shape; the reference build does not keep a
    // curated snapshot cache, so cold boot is used when no warm VM is claimed.
    let snapshot_available = false;
    let instance = match state.local.place(&spec, snapshot_available).await {
        Ok(i) => i,
        Err(e) => {
            sb.state = State::Failed;
            sb.error = Some(format!("boot failed: {e}"));
            sb.updated_at = Utc::now();
            state.store.put_sandbox(&sb).ok();
            return Err(ApiError::Internal(e));
        }
    };

    sb.runtime_handle = Some(instance.handle.clone());
    sb.boot_path = instance.boot_path;
    sb.state = State::Running;
    sb.timings.boot_ms = instance.boot_ms;
    sb.timings.image_cache_ms = instance.image_cache_ms;
    sb.timings.browser_ready_ms = instance.browser_ready_ms;

    // --- startup recipe (spec §14) --------------------------------------
    if let Some(recipe) = &startup {
        run_startup(state, &mut sb, recipe).await;
    }

    // Browser exposes noVNC (6080) and CDP (9222) preview routes.
    if browser.is_some() {
        for p in [6080u16, 9222u16] {
            if !sb.ports.contains(&p) {
                let _ = state.local.expose_port(instance.handle.as_str(), p).await;
                sb.ports.push(p);
            }
        }
    }

    sb.timings.total_ms = wall_start.elapsed().as_millis() as u64;
    sb.updated_at = Utc::now();
    sb.last_active_at = Utc::now();
    state.store.put_sandbox(&sb).map_err(ApiError::Internal)?;

    // --- open a billing interval (spec §22) -----------------------------
    open_usage(state, &sb);

    Ok(sb)
}

/// Enforce the image's minimum resources (spec §3.4 explicit heavier path,
/// §10.1, §12.1).
fn enforce_minimums(class: &ImageClass, r: &Resources) -> ApiResult<()> {
    let min = class.minimum_resources();
    if r.cpu + 1e-9 < min.cpu || r.memory_mb < min.memory_mb || r.disk_gb < min.disk_gb {
        return Err(ApiError::BadRequest(format!(
            "image '{}' requires at least {} vCPU / {} MB / {} GB; request explicit resources",
            class.key(),
            min.cpu,
            min.memory_mb,
            min.disk_gb
        )));
    }
    Ok(())
}

/// Accept either `"none"` or a structured recipe object.
fn parse_startup(value: Option<serde_json::Value>) -> ApiResult<Option<StartupRecipe>> {
    match value {
        None => Ok(None),
        Some(serde_json::Value::String(s)) if s == "none" => Ok(None),
        Some(serde_json::Value::Null) => Ok(None),
        Some(v) => {
            let recipe: StartupRecipe = serde_json::from_value(v)
                .map_err(|e| ApiError::BadRequest(format!("invalid startup recipe: {e}")))?;
            Ok(Some(recipe))
        }
    }
}

/// Run git clone, package cache warmup, commands, port preview, and ready check,
/// recording each phase's timing separately (spec §14 feature table).
async fn run_startup(state: &AppState, sb: &mut Sandbox, recipe: &StartupRecipe) {
    let handle = match &sb.runtime_handle {
        Some(h) => h.clone(),
        None => return,
    };

    // git clone (shallow) ------------------------------------------------
    if let Some(git) = &recipe.git {
        let t = Instant::now();
        let cmd = format!(
            "git clone --depth {} --branch {} {} . 2>&1 || git clone --depth {} {} .",
            git.depth, git.r#ref, shell_arg(&git.url), git.depth, shell_arg(&git.url)
        );
        let res = state
            .local
            .exec(&handle, &ExecRequest { cmd, cwd: None, env: BTreeMap::new(), background: false })
            .await;
        sb.timings.git_ms = t.elapsed().as_millis() as u64;
        if let Ok(r) = &res {
            if r.exit_code != 0 {
                sb.error = Some(format!("git clone exit {}: {}", r.exit_code, r.stderr));
            }
        } else if let Err(e) = res {
            sb.error = Some(format!("git clone failed: {e}"));
        }
    }

    // commands (install etc.) -------------------------------------------
    let t = Instant::now();
    for cmd in &recipe.commands {
        let _ = state
            .local
            .exec(
                &handle,
                &ExecRequest {
                    cmd: cmd.run.clone(),
                    cwd: None,
                    env: recipe.env.clone(),
                    background: cmd.background,
                },
            )
            .await;
    }
    if !recipe.commands.is_empty() {
        sb.timings.install_ms = t.elapsed().as_millis() as u64;
    }

    // port preview -------------------------------------------------------
    for &port in &recipe.ports {
        if state.local.expose_port(&handle, port).await.is_ok() && !sb.ports.contains(&port) {
            sb.ports.push(port);
        }
    }

    // ready check --------------------------------------------------------
    if let Some(ready) = &recipe.ready {
        if let Some(url) = &ready.http {
            let t = Instant::now();
            let _ = state.local.ready_check(&handle, url, ready.timeout_seconds).await;
            sb.timings.ready_ms = t.elapsed().as_millis() as u64;
        }
    }
}

fn shell_arg(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Gather scheduler snapshots for every registered node. The reference build
/// can only execute on the local node, but the scheduler still scores all of
/// them (registry/scoring is multi-node; execution is single-node).
async fn gather_node_snapshots(
    state: &AppState,
    req: &PlacementRequest,
) -> ApiResult<Vec<NodeSnapshot>> {
    let nodes = state.store.list_nodes().map_err(ApiError::Internal)?;
    let mut out = vec![];
    for node in nodes {
        let active = state
            .store
            .active_sandboxes_on_node(&node.id)
            .map_err(ApiError::Internal)?;
        let used_memory_gb = crate::store::active_memory_gb(&active);
        let active_count = active.len() as u32;
        let org_active_count = active.iter().filter(|s| s.org_id == req.org_id).count() as u32;
        let hot_pool_available = if node.id == state.local_node_id {
            state.local.hot_pool_available(&req.image_key, &req.resources).await
        } else {
            0
        };
        // Synthetic pressure from occupancy (real nodes export measured values).
        let occupancy = (used_memory_gb / node.capacity().usable_for_sandboxes_gb.max(1.0)).clamp(0.0, 1.0);
        out.push(NodeSnapshot {
            node: node.clone(),
            used_memory_gb,
            active_count,
            org_active_count,
            hot_pool_available,
            image_cached: !req.is_custom_image, // curated always cached locally
            snapshot_available: false,
            cpu_pressure: occupancy * 0.7,
            io_pressure: occupancy * 0.5,
            custom_image_cache_pressure: 0.0,
        });
    }
    Ok(out)
}

/// Resolve `startup.secrets` (names) to decrypted values for late injection,
/// returning (values_by_name, names). Values never touch the persisted record.
fn resolve_secrets(
    state: &AppState,
    ctx: &AuthContext,
    startup: Option<&StartupRecipe>,
) -> ApiResult<(BTreeMap<String, String>, Vec<String>)> {
    let mut values = BTreeMap::new();
    let mut names = vec![];
    if let Some(s) = startup {
        for name in &s.secrets {
            let rec = state
                .store
                .get_secret(&ctx.org_id, name)
                .map_err(ApiError::Internal)?
                .ok_or_else(|| ApiError::BadRequest(format!("secret '{name}' is not defined for this org")))?;
            let value = crate::secrets::decrypt(&state.secret_key, &rec)
                .map_err(|e| ApiError::Internal(anyhow::anyhow!("decrypt secret '{name}': {e}")))?;
            values.insert(name.clone(), value);
            names.push(name.clone());
        }
    }
    Ok((values, names))
}

/// Decode inline ephemeral files (utf8 or base64) for writing into the workspace.
fn build_ephemeral_files(files: Option<&[EphemeralFile]>) -> ApiResult<Vec<(String, Vec<u8>)>> {
    let mut out = vec![];
    for f in files.unwrap_or(&[]) {
        let bytes = match f.encoding.as_deref() {
            Some("base64") => base64_decode(&f.content)
                .map_err(|e| ApiError::BadRequest(format!("file '{}': {e}", f.path)))?,
            _ => f.content.clone().into_bytes(),
        };
        out.push((f.path.clone(), bytes));
    }
    Ok(out)
}

fn validate_mounts(mounts: &[MountSpec]) -> ApiResult<()> {
    for m in mounts {
        if m.kind != "s3" {
            return Err(ApiError::BadRequest(format!("unsupported mount type '{}' (only 's3')", m.kind)));
        }
        if m.bucket.is_empty() {
            return Err(ApiError::BadRequest("mount requires a bucket".into()));
        }
        if !m.mount_path.starts_with('/') {
            return Err(ApiError::BadRequest(format!("mount_path '{}' must be absolute", m.mount_path)));
        }
    }
    Ok(())
}

fn base64_decode(input: &str) -> Result<Vec<u8>, String> {
    const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut table = [255u8; 256];
    for (i, &c) in B64.iter().enumerate() {
        table[c as usize] = i as u8;
    }
    let clean: Vec<u8> = input.bytes().filter(|&b| b != b'=' && !b.is_ascii_whitespace()).collect();
    let mut out = Vec::new();
    for chunk in clean.chunks(4) {
        let mut acc = 0u32;
        let mut bits = 0;
        for &c in chunk {
            let v = table[c as usize];
            if v == 255 {
                return Err("invalid base64".into());
            }
            acc = (acc << 6) | v as u32;
            bits += 6;
        }
        let bytes = bits / 8;
        acc <<= 24 - bits;
        for i in 0..bytes {
            out.push((acc >> (16 - i * 8)) as u8);
        }
    }
    Ok(out)
}

fn org_active_units(state: &AppState, org_id: &str) -> ApiResult<f64> {
    let sandboxes = state.store.list_sandboxes_for_org(org_id).map_err(ApiError::Internal)?;
    Ok(sandboxes
        .iter()
        .filter(|s| s.state.is_active())
        .map(|s| units_for_memory_gb(s.resources.memory_gb()))
        .sum())
}

// --- usage accounting -------------------------------------------------------

pub fn open_usage(state: &AppState, sb: &Sandbox) {
    let class = classify(&sb.image).unwrap_or(ImageClass::Base);
    let q = pricing::quote(&state.cfg.pricing, &sb.resources, &class);
    let iv = UsageInterval {
        id: ids::build_id(),
        sandbox_id: sb.id.clone(),
        org_id: sb.org_id.clone(),
        resources: sb.resources,
        image_key: class.key().to_string(),
        resource_units: q.resource_units,
        image_multiplier: q.image_multiplier,
        unit_price_usd_hr: q.unit_price_usd_hr,
        started_at: Utc::now(),
        ended_at: None,
    };
    // Guard against a second interval for the same sandbox (review #2).
    state.store.open_usage_if_none(&iv).ok();
}

pub fn close_usage(state: &AppState, sandbox_id: &str) {
    state.store.close_open_usage(sandbox_id, Utc::now()).ok();
    // Draw down the org balance by what the now-closed intervals cost.
    if let Ok(Some(sb)) = state.store.get_sandbox(sandbox_id) {
        recompute_org_spend(state, &sb.org_id);
    }
}

fn recompute_org_spend(state: &AppState, org_id: &str) {
    if let Ok(Some(mut org)) = state.store.get_org(org_id) {
        let now = Utc::now();
        let spent: f64 = state
            .store
            .usage_for_org(org_id)
            .unwrap_or_default()
            .iter()
            .map(|iv| iv.cost_usd(now))
            .sum();
        org.spent_usd = spent;
        state.store.put_org(&org).ok();
    }
}

// --- lifecycle operations ---------------------------------------------------

pub async fn stop_sandbox(state: &AppState, sb: Sandbox) -> ApiResult<Sandbox> {
    // CAS Running -> Stopping; a concurrent stop/delete/reaper loses the race
    // and we no-op (review #3). Idempotent if already stopped.
    let updated = match state
        .store
        .cas_sandbox(&sb.id, &[State::Running], |s| s.state = State::Stopping)
        .map_err(ApiError::Internal)?
    {
        CasOutcome::Updated(s) => s,
        CasOutcome::Conflict(State::Stopped) | CasOutcome::Conflict(State::Stopping) => return Ok(sb),
        CasOutcome::Conflict(other) => {
            return Err(ApiError::Conflict(format!("cannot stop from {}", other.as_str())))
        }
        CasOutcome::NotFound => return Err(ApiError::NotFound(format!("sandbox {}", sb.id))),
    };

    // Billing stops the moment the user asked to stop, regardless of whether the
    // runtime pause then succeeds (review #4).
    close_usage(state, &updated.id);

    let handle = updated.runtime_handle.clone().unwrap_or_default();
    let pause_result = state.local.pause(&handle, updated.snapshot_enabled).await;
    let next = if pause_result.is_ok() { State::Stopped } else { State::Failed };
    let err = pause_result.as_ref().err().map(|e| format!("pause failed: {e}"));
    let final_sb = state
        .store
        .cas_sandbox(&updated.id, &[State::Stopping], |s| {
            s.state = next;
            s.error = err.clone();
            s.updated_at = Utc::now();
        })
        .map_err(ApiError::Internal)?;
    match final_sb {
        CasOutcome::Updated(s) => Ok(s),
        _ => Ok(updated), // someone else advanced it; return what we have
    }
}

pub async fn resume_sandbox(state: &AppState, sb: Sandbox) -> ApiResult<Sandbox> {
    // CAS Stopped -> Resuming; only one concurrent resume wins (review #2).
    let resuming = match state
        .store
        .cas_sandbox(&sb.id, &[State::Stopped], |s| s.state = State::Resuming)
        .map_err(ApiError::Internal)?
    {
        CasOutcome::Updated(s) => s,
        CasOutcome::Conflict(other) => {
            return Err(ApiError::Conflict(format!("cannot resume from {}", other.as_str())))
        }
        CasOutcome::NotFound => return Err(ApiError::NotFound(format!("sandbox {}", sb.id))),
    };
    let handle = resuming.runtime_handle.clone().unwrap_or_default();
    let resume_ms = match state.local.resume(&handle).await {
        Ok(ms) => ms,
        Err(e) => {
            state
                .store
                .cas_sandbox(&resuming.id, &[State::Resuming], |s| {
                    s.state = State::Failed;
                    s.error = Some(format!("resume failed: {e}"));
                })
                .ok();
            return Err(ApiError::Internal(e));
        }
    };
    let final_sb = state
        .store
        .cas_sandbox(&resuming.id, &[State::Resuming], |s| {
            s.state = State::Running;
            s.timings.ready_ms = resume_ms;
            s.updated_at = Utc::now();
            s.last_active_at = Utc::now();
        })
        .map_err(ApiError::Internal)?;
    let running = match final_sb {
        CasOutcome::Updated(s) => s,
        _ => resuming,
    };
    open_usage(state, &running);
    Ok(running)
}

pub async fn delete_sandbox(state: &AppState, sb: Sandbox) -> ApiResult<()> {
    // CAS any non-terminal state -> Deleting.
    let deleting = match state
        .store
        .cas_sandbox(
            &sb.id,
            &[State::Creating, State::Running, State::Resuming, State::Stopping, State::Stopped, State::Failed],
            |s| s.state = State::Deleting,
        )
        .map_err(ApiError::Internal)?
    {
        CasOutcome::Updated(s) => s,
        CasOutcome::Conflict(State::Deleting) | CasOutcome::Conflict(State::Deleted) => return Ok(()),
        CasOutcome::Conflict(other) => {
            return Err(ApiError::Conflict(format!("cannot delete from {}", other.as_str())))
        }
        CasOutcome::NotFound => return Err(ApiError::NotFound(format!("sandbox {}", sb.id))),
    };

    // Stop billing before the runtime teardown (review #4).
    close_usage(state, &deleting.id);

    let handle = deleting.runtime_handle.clone().unwrap_or_default();
    if !handle.is_empty() {
        if let Err(e) = state.local.delete(&handle).await {
            // Don't block the user's delete on a runtime hiccup; mark deleted and
            // log the potential leak for a sweeper to reclaim.
            tracing::warn!(sandbox = %deleting.id, error = %e, "runtime delete failed; marking deleted anyway");
        }
    }
    state
        .store
        .cas_sandbox(&deleting.id, &[State::Deleting], |s| {
            s.state = State::Deleted;
            s.updated_at = Utc::now();
        })
        .map_err(ApiError::Internal)?;
    Ok(())
}

pub async fn snapshot_sandbox(state: &AppState, sb: &Sandbox) -> ApiResult<Snapshot> {
    // Never capture resident secrets into a snapshot (review M3). Production
    // could scrub-then-snapshot; the safe default is to refuse.
    if !sb.secret_names.is_empty() {
        return Err(ApiError::Conflict(
            "cannot snapshot a sandbox with resident secrets; remove secrets and retry".into(),
        ));
    }
    let handle = sb.runtime_handle.clone().unwrap_or_default();
    let artifact = state.local.snapshot(&handle).await.map_err(ApiError::Internal)?;
    let snap = Snapshot {
        id: artifact.handle.clone(),
        sandbox_id: sb.id.clone(),
        org_id: sb.org_id.clone(),
        image: sb.image.clone(),
        created_at: Utc::now(),
        storage_bytes: artifact.storage_bytes,
        handle: artifact.handle,
    };
    state.store.put_snapshot(&snap).map_err(ApiError::Internal)?;
    Ok(snap)
}

/// Touch activity so the idle detector does not auto-stop a busy sandbox.
/// Updates only `last_active_at` (no read-modify-write) to avoid clobbering a
/// concurrent state change (review #6, #7).
pub fn touch_activity(state: &AppState, sb: &mut Sandbox) {
    let now = Utc::now();
    sb.last_active_at = now;
    state.store.touch_last_active(&sb.id, now).ok();
}
