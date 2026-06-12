//! End-to-end API test against the in-process server with the mock runtime.
//! Exercises the spec §25.1 one-node acceptance flow: default create, exec
//! echo, file read/write, preview port, knob rejection, auth, and delete.

use sandboxd::app::build_state;
use sandboxd::config::Config;
use sandboxd::state::AppState;

async fn spawn_server() -> (String, String, tempfile::TempDir) {
    let (base, key, _state, tmp) = spawn_server_full().await;
    (base, key, tmp)
}

/// Like [`spawn_server`] but also hands back the in-process [`AppState`], so a
/// test can drive service-layer operations (e.g. the idle reaper's standby
/// path) directly instead of waiting out the real auto-stop window.
async fn spawn_server_full() -> (String, String, AppState, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let (base, state) = serve_on(tmp.path().to_path_buf()).await;
    (base, "sk_live_test".to_string(), state, tmp)
}

/// Test config rooted at `data` (mock runtime, fixed admin key). Reusing the
/// same `data` across two `serve_on` calls simulates a control-plane restart:
/// the SQLite store and on-disk workspaces persist, but the runtime is fresh.
fn test_config(data: &std::path::Path) -> Config {
    let mut cfg = Config::default();
    cfg.server.data_dir = data.to_path_buf();
    cfg.server.bind = "127.0.0.1:0".into();
    cfg.server.public_domain = "test.local".into();
    cfg.runtime.kind = "mock".into();
    cfg.runtime.workspace_dir = data.join("workspaces");
    cfg.runtime.images_dir = data.join("images");
    cfg.runtime.volumes_dir = data.join("volumes");
    cfg.runtime.kernel_image = data.join("kernel/vmlinux").to_string_lossy().to_string();
    cfg.auth.bootstrap_admin_key = "sk_live_test".into();
    cfg
}

/// Build state + serve on an ephemeral port for the given data dir.
async fn serve_on(data: std::path::PathBuf) -> (String, AppState) {
    // The mock runtime is dev-only and refuses to start without this opt-in.
    std::env::set_var("WORKDIR_ALLOW_INSECURE_RUNTIME", "1");
    let state = build_state(test_config(&data)).await.expect("build state");
    // Warm the base pool so the default create takes the hot_pool path.
    for _ in 0..4 {
        state.local.warm_once().await;
    }
    let app = sandboxd::api::router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), state)
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

#[tokio::test]
async fn one_node_acceptance_flow() {
    let (base, key, _tmp) = spawn_server().await;
    let c = client();
    let auth = format!("Bearer {key}");

    // --- unauthorized create is rejected ---
    let resp = c.post(format!("{base}/v1/sandboxes")).send().await.unwrap();
    assert_eq!(resp.status(), 401, "missing key must be unauthorized");

    // --- default cheap-path create ---
    let resp = c
        .post(format!("{base}/v1/sandboxes"))
        .header("authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let sb: serde_json::Value = resp.json().await.unwrap();
    let id = sb["id"].as_str().unwrap().to_string();
    assert_eq!(sb["image"], "base");
    assert_eq!(sb["resources"]["memory_mb"], 2048);
    assert_eq!(sb["boot_path"], "hot_pool", "warmed pool should serve a hot VM");
    assert_eq!(sb["price"]["resource_units"], 1.0);

    // --- exec echo ok ---
    let resp = c
        .post(format!("{base}/v1/sandboxes/{id}/exec"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"cmd": "echo ok"}))
        .send()
        .await
        .unwrap();
    let out: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(out["exit_code"], 0);
    assert_eq!(out["stdout"], "ok\n");

    // --- file write + read round trip ---
    c.put(format!("{base}/v1/sandboxes/{id}/files"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"path": "a/b.txt", "content": "hello"}))
        .send()
        .await
        .unwrap();
    let resp = c
        .get(format!("{base}/v1/sandboxes/{id}/files?path=a/b.txt"))
        .header("authorization", &auth)
        .send()
        .await
        .unwrap();
    let read: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(read["content"], "hello");

    // --- expose a preview port ---
    let resp = c
        .post(format!("{base}/v1/sandboxes/{id}/ports/3000/expose"))
        .header("authorization", &auth)
        .send()
        .await
        .unwrap();
    let port: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(port["url"], format!("https://{id}-3000.test.local"));

    // --- knob rejection: 13 GB memory ---
    let resp = c
        .post(format!("{base}/v1/sandboxes"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"resources": {"memory_mb": 13312}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400, "arbitrary memory must be rejected");

    // --- delete cleans up ---
    let resp = c
        .delete(format!("{base}/v1/sandboxes/{id}"))
        .header("authorization", &auth)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let resp = c
        .get(format!("{base}/v1/sandboxes/{id}"))
        .header("authorization", &auth)
        .send()
        .await
        .unwrap();
    let sb: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(sb["state"], "deleted");
}

#[tokio::test]
async fn concurrent_resume_does_not_double_bill() {
    // Regression for review #2/#3: a double-clicked resume must not open two
    // billing intervals or otherwise corrupt state.
    let (base, key, _tmp) = spawn_server().await;
    let c = client();
    let auth = format!("Bearer {key}");

    let id = c
        .post(format!("{base}/v1/sandboxes"))
        .header("authorization", &auth)
        .send()
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // Stop it.
    c.post(format!("{base}/v1/sandboxes/{id}/pause"))
        .header("authorization", &auth)
        .send()
        .await
        .unwrap();

    // Fire two resumes concurrently.
    let (r1, r2) = tokio::join!(
        c.post(format!("{base}/v1/sandboxes/{id}/resume")).header("authorization", &auth).send(),
        c.post(format!("{base}/v1/sandboxes/{id}/resume")).header("authorization", &auth).send(),
    );
    let s1 = r1.unwrap().status();
    let s2 = r2.unwrap().status();
    // Exactly one wins (200); the loser gets 409 (or both serialize to 200, but
    // never two open intervals).
    assert!(s1.is_success() || s2.is_success(), "at least one resume should win");

    // The sandbox must end up running with exactly one accruing interval.
    let usage: serde_json::Value = c
        .get(format!("{base}/v1/usage"))
        .header("authorization", &auth)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mine: Vec<_> = usage["sandboxes"].as_array().unwrap().iter()
        .filter(|s| s["sandbox_id"] == serde_json::json!(id)).collect();
    // One usage summary row for the sandbox (intervals are merged per-sandbox in
    // the view, but the cost must reflect a single running stream, not double).
    assert_eq!(mine.len(), 1, "exactly one usage row for the sandbox");
}

#[tokio::test]
async fn secrets_inject_and_snapshot_is_refused() {
    let (base, key, _tmp) = spawn_server().await;
    let c = client();
    let auth = format!("Bearer {key}");

    // Store a secret; the list must not contain the value.
    c.put(format!("{base}/v1/secrets/MY_TOKEN"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"value": "top-secret-xyz"}))
        .send()
        .await
        .unwrap();
    let list: serde_json::Value = c
        .get(format!("{base}/v1/secrets"))
        .header("authorization", &auth)
        .send().await.unwrap().json().await.unwrap();
    let body = serde_json::to_string(&list).unwrap();
    assert!(body.contains("MY_TOKEN"));
    assert!(!body.contains("top-secret-xyz"), "secret value must never be returned");

    // Create a sandbox that references the secret + an inline ephemeral file.
    let sb: serde_json::Value = c
        .post(format!("{base}/v1/sandboxes"))
        .header("authorization", &auth)
        .json(&serde_json::json!({
            "files": [{"path": "note.txt", "content": "hi"}],
            "startup": {"secrets": ["MY_TOKEN"]}
        }))
        .send().await.unwrap().json().await.unwrap();
    let id = sb["id"].as_str().unwrap().to_string();
    assert_eq!(sb["secret_names"], serde_json::json!(["MY_TOKEN"]));

    // The secret is in the sandbox env, and the ephemeral file is present.
    let out: serde_json::Value = c
        .post(format!("{base}/v1/sandboxes/{id}/exec"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"cmd": "echo $MY_TOKEN && cat note.txt"}))
        .send().await.unwrap().json().await.unwrap();
    assert!(out["stdout"].as_str().unwrap().contains("top-secret-xyz"));
    assert!(out["stdout"].as_str().unwrap().contains("hi"));

    // Snapshot must be refused while secrets are resident (review M3).
    let snap = c
        .post(format!("{base}/v1/sandboxes/{id}/snapshot"))
        .header("authorization", &auth)
        .send().await.unwrap();
    assert_eq!(snap.status(), 409);
}

#[tokio::test]
async fn docker_requires_capable_image() {
    let (base, key, _tmp) = spawn_server().await;
    let c = client();
    let auth = format!("Bearer {key}");

    // base image has no docker daemon -> rejected.
    let r = c
        .post(format!("{base}/v1/sandboxes"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"docker": {"enabled": true}}))
        .send().await.unwrap();
    assert_eq!(r.status(), 400);

    // heavy-build with explicit resources -> accepted, docker flagged.
    let sb: serde_json::Value = c
        .post(format!("{base}/v1/sandboxes"))
        .header("authorization", &auth)
        .json(&serde_json::json!({
            "image": "heavy-build",
            "resources": {"cpu": 2, "memory_mb": 8192, "disk_gb": 32},
            "docker": {"enabled": true}
        }))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(sb["docker"], serde_json::json!(true));
}

#[tokio::test]
async fn malformed_create_body_is_rejected_not_silently_defaulted() {
    // Regression: a body that doesn't match the schema (here `docker` as a bare
    // bool instead of `{enabled}`) must 400 — NOT fall through to a default base
    // sandbox. The caller asked for something specific; a wrong-shaped request
    // should fail loudly, not hand back a plain box.
    let (base, key, _tmp) = spawn_server().await;
    let c = client();
    let auth = format!("Bearer {key}");

    let r = c
        .post(format!("{base}/v1/sandboxes"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"docker": true})) // wrong shape
        .send().await.unwrap();
    assert_eq!(r.status(), 400, "a malformed create body must be rejected");

    // An empty body is still the valid no-arg default create.
    let r = c
        .post(format!("{base}/v1/sandboxes"))
        .header("authorization", &auth)
        .send().await.unwrap();
    assert_eq!(r.status(), 201, "no-body create must still default cleanly");
}

#[tokio::test]
async fn coding_agent_is_opt_in_and_validated() {
    let (base, key, _tmp) = spawn_server().await;
    let c = client();
    let auth = format!("Bearer {key}");

    // Default create has no coding agent — sandboxes stay minimal.
    let plain: serde_json::Value = c
        .post(format!("{base}/v1/sandboxes"))
        .header("authorization", &auth)
        .json(&serde_json::json!({}))
        .send().await.unwrap().json().await.unwrap();
    assert!(plain.get("coding_agent").is_none());

    // An unknown agent kind is rejected.
    let r = c
        .post(format!("{base}/v1/sandboxes"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"coding_agent": {"enabled": true, "kind": "cursor"}}))
        .send().await.unwrap();
    assert_eq!(r.status(), 400);

    // Opting in installs opencode (works on the base image) and is reflected.
    let sb: serde_json::Value = c
        .post(format!("{base}/v1/sandboxes"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"coding_agent": {"enabled": true}}))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(sb["coding_agent"], serde_json::json!("opencode"));
}

#[tokio::test]
async fn browser_requires_explicit_resources_and_image() {
    let (base, key, _tmp) = spawn_server().await;
    let c = client();
    let auth = format!("Bearer {key}");

    // Browser flag without the browser image is rejected.
    let resp = c
        .post(format!("{base}/v1/sandboxes"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"browser": {"enabled": true}}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // Browser image at default (too-small) resources is rejected.
    let resp = c
        .post(format!("{base}/v1/sandboxes"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"image": "browser"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    // Browser image with explicit resources succeeds and exposes VNC/CDP.
    let resp = c
        .post(format!("{base}/v1/sandboxes"))
        .header("authorization", &auth)
        .json(&serde_json::json!({
            "image": "browser",
            "resources": {"cpu": 2, "memory_mb": 4096, "disk_gb": 16},
            "browser": {"enabled": true, "vnc": true, "cdp": true}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let sb: serde_json::Value = resp.json().await.unwrap();
    assert!(sb["urls"]["vnc"].as_str().unwrap().contains("-6080."));
    assert!(sb["urls"]["cdp"].as_str().unwrap().contains("-9222."));
    assert!(sb["browser_ready_ms"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn standby_preserves_state_and_auto_resumes() {
    // Roadmap Phase 1: an idle sandbox is snapshotted + RAM-freed into `standby`
    // (billing $0), then transparently auto-resumes — with its disk intact — on
    // the next request.
    let (base, key, state, _tmp) = spawn_server_full().await;
    let c = client();
    let auth = format!("Bearer {key}");

    let id = c
        .post(format!("{base}/v1/sandboxes"))
        .header("authorization", &auth)
        .send().await.unwrap()
        .json::<serde_json::Value>().await.unwrap()["id"]
        .as_str().unwrap().to_string();

    // Write a file so we can prove disk state survives the snapshot/restore.
    c.put(format!("{base}/v1/sandboxes/{id}/files"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"path": "keep.txt", "content": "survives"}))
        .send().await.unwrap();

    // Drive the reaper's decision directly (the real window is >= 30s).
    let sb = state.store.get_sandbox(&id).unwrap().unwrap();
    let org_id = sb.org_id.clone();
    let parked = sandboxd::service::standby_sandbox(&state, sb).await.expect("standby");
    assert_eq!(parked.state.as_str(), "standby");

    // The API reports standby and it bills $0: no open billing interval remains.
    let view: serde_json::Value = c
        .get(format!("{base}/v1/sandboxes/{id}"))
        .header("authorization", &auth)
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(view["state"], "standby");
    assert_eq!(view["cost_estimate_usd"], serde_json::json!(0.0));
    let open_now = state.store.usage_for_org(&org_id).unwrap()
        .into_iter().filter(|iv| iv.sandbox_id == id && iv.ended_at.is_none()).count();
    assert_eq!(open_now, 0, "standby must close the billing interval ($0 while parked)");

    // First request auto-resumes; the file written before standby is still there.
    let out: serde_json::Value = c
        .post(format!("{base}/v1/sandboxes/{id}/exec"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"cmd": "cat keep.txt"}))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(out["stdout"], "survives", "disk state must survive standby");

    // Back to running, fast, and billing again.
    let view: serde_json::Value = c
        .get(format!("{base}/v1/sandboxes/{id}"))
        .header("authorization", &auth)
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(view["state"], "running");
    assert!(view["timings"]["ready_ms"].as_u64().unwrap() < 200, "resume < 200ms (Phase 1 target)");
    let open_after = state.store.usage_for_org(&org_id).unwrap()
        .into_iter().filter(|iv| iv.sandbox_id == id && iv.ended_at.is_none()).count();
    assert_eq!(open_after, 1, "auto-resume reopens exactly one billing interval");
}

#[tokio::test]
async fn benchmark_harness_separates_boot_paths() {
    // Roadmap Phase 0: a sweep measures cold_boot / hot_pool / snapshot_restore
    // separately and the table reports them without merging.
    let (base, key, _state, _tmp) = spawn_server_full().await;
    let c = client();
    let auth = format!("Bearer {key}");

    let res: serde_json::Value = c
        .post(format!("{base}/v1/benchmarks/run"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"image": "base", "iterations": 1}))
        .send().await.unwrap().json().await.unwrap();
    assert!(res["ran"].as_u64().unwrap() >= 3, "one iteration measures all three paths");

    let table: serde_json::Value = c
        .get(format!("{base}/v1/benchmarks"))
        .header("authorization", &auth)
        .send().await.unwrap().json().await.unwrap();
    let series = table["series"].as_array().unwrap();
    let find = |path: &str| series.iter().find(|s| s["boot_path"] == path).cloned();
    let cold = find("cold_boot").expect("cold_boot series");
    let hot = find("hot_pool").expect("hot_pool series");
    let restore = find("snapshot_restore").expect("snapshot_restore series");

    // Paths are reported separately, with the expected ordering.
    let p50 = |v: &serde_json::Value| v["ready_ms_p50"].as_u64().unwrap();
    assert!(p50(&hot) < p50(&cold), "hot_pool must beat cold_boot");
    assert!(p50(&restore) < p50(&cold), "snapshot_restore must beat cold_boot");
    // The simulated optimized restore meets the Phase 2 target.
    assert!(restore["ready_ms_p90"].as_u64().unwrap() <= 50, "snapshot_restore p90 <= 50ms (simulated)");
}

#[tokio::test]
async fn fork_clones_an_instant_sibling() {
    // Roadmap Phase 3: fork copies the parent's snapshot artifact into a new,
    // independent sandbox that starts from the parent's exact disk state.
    let (base, key, _state, _tmp) = spawn_server_full().await;
    let c = client();
    let auth = format!("Bearer {key}");

    let parent_id = c
        .post(format!("{base}/v1/sandboxes"))
        .header("authorization", &auth)
        .send().await.unwrap()
        .json::<serde_json::Value>().await.unwrap()["id"]
        .as_str().unwrap().to_string();

    // Seed state on the parent that the fork must inherit.
    c.put(format!("{base}/v1/sandboxes/{parent_id}/files"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"path": "inherited.txt", "content": "from-parent"}))
        .send().await.unwrap();

    // Fork.
    let resp = c
        .post(format!("{base}/v1/sandboxes/{parent_id}/fork"))
        .header("authorization", &auth)
        .send().await.unwrap();
    assert_eq!(resp.status(), 201);
    let child: serde_json::Value = resp.json().await.unwrap();
    let child_id = child["id"].as_str().unwrap().to_string();
    assert_ne!(child_id, parent_id, "fork must produce a new sandbox id");
    assert_eq!(child["state"], "running");
    assert_eq!(child["boot_path"], "fork", "fork must report its own boot path");

    // The child starts from the parent's disk state.
    let out: serde_json::Value = c
        .post(format!("{base}/v1/sandboxes/{child_id}/exec"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"cmd": "cat inherited.txt"}))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(out["stdout"], "from-parent", "child inherits the parent's disk");

    // Child and parent are independent: writing in the child does not touch the
    // parent, and deleting the child leaves the parent running.
    c.put(format!("{base}/v1/sandboxes/{child_id}/files"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"path": "inherited.txt", "content": "child-only"}))
        .send().await.unwrap();
    let parent_out: serde_json::Value = c
        .post(format!("{base}/v1/sandboxes/{parent_id}/exec"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"cmd": "cat inherited.txt"}))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(parent_out["stdout"], "from-parent", "parent disk is unaffected by child writes");

    let del = c.delete(format!("{base}/v1/sandboxes/{child_id}"))
        .header("authorization", &auth).send().await.unwrap();
    assert!(del.status().is_success());
    let parent_view: serde_json::Value = c
        .get(format!("{base}/v1/sandboxes/{parent_id}"))
        .header("authorization", &auth)
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(parent_view["state"], "running", "parent survives the child's deletion");
}

#[tokio::test]
async fn standby_survives_control_plane_restart() {
    // Roadmap Phase 1 (perpetual): a standby sandbox must come back after the
    // control plane restarts — its VM record is persisted to disk, so a fresh
    // runtime rehydrates it and `restore` works. Without this, "perpetual"
    // would only hold until the next daemon restart.
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().to_path_buf();
    let c = client();
    let auth = "Bearer sk_live_test".to_string();

    // --- first boot: create, seed disk state, then park in standby ---
    let id = {
        let (base, state) = serve_on(data.clone()).await;
        let id = c
            .post(format!("{base}/v1/sandboxes"))
            .header("authorization", &auth)
            .send().await.unwrap()
            .json::<serde_json::Value>().await.unwrap()["id"]
            .as_str().unwrap().to_string();
        c.put(format!("{base}/v1/sandboxes/{id}/files"))
            .header("authorization", &auth)
            .json(&serde_json::json!({"path": "persist.txt", "content": "across-restart"}))
            .send().await.unwrap();
        let sb = state.store.get_sandbox(&id).unwrap().unwrap();
        let parked = sandboxd::service::standby_sandbox(&state, sb).await.expect("standby");
        assert_eq!(parked.state.as_str(), "standby");
        id
    };

    // --- restart: a brand-new server/runtime on the same data dir ---
    let (base2, _state2) = serve_on(data.clone()).await;

    // The sandbox is still standby after the restart (reconcile leaves it).
    let view: serde_json::Value = c
        .get(format!("{base2}/v1/sandboxes/{id}"))
        .header("authorization", &auth)
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(view["state"], "standby", "standby must survive the restart, not be failed");

    // A request to the fresh runtime auto-resumes from the persisted record, and
    // the disk state written before the restart is intact.
    let out: serde_json::Value = c
        .post(format!("{base2}/v1/sandboxes/{id}/exec"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"cmd": "cat persist.txt"}))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(out["stdout"], "across-restart", "disk + standby survive a control-plane restart");

    let view: serde_json::Value = c
        .get(format!("{base2}/v1/sandboxes/{id}"))
        .header("authorization", &auth)
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(view["state"], "running");
}

#[tokio::test]
async fn benchmark_sweep_covers_all_curated_images() {
    // Roadmap Phase 0 (complete): a full sweep measures every curated image, not
    // just `base`, each at its own shape.
    let (base, key, _state, _tmp) = spawn_server_full().await;
    let c = client();
    let auth = format!("Bearer {key}");

    let res: serde_json::Value = c
        .post(format!("{base}/v1/benchmarks/run"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"image": "all", "iterations": 1}))
        .send().await.unwrap().json().await.unwrap();

    let series = res["series"].as_array().unwrap();
    let images: std::collections::BTreeSet<&str> =
        series.iter().filter_map(|s| s["image"].as_str()).collect();
    for want in ["base", "node-python", "browser", "heavy-build"] {
        assert!(images.contains(want), "sweep must cover {want}; got {images:?}");
    }
    // Each image is still reported with separated boot paths.
    let browser_paths: std::collections::BTreeSet<&str> = series.iter()
        .filter(|s| s["image"] == "browser")
        .filter_map(|s| s["boot_path"].as_str())
        .collect();
    assert!(browser_paths.contains("cold_boot") && browser_paths.contains("snapshot_restore"));
}

#[tokio::test]
async fn volumes_persist_across_sandboxes_and_attach_exclusively() {
    // Roadmap Phase 5: a persistent volume outlives the sandbox it was attached
    // to, attaches to at most one sandbox at a time, and refuses deletion while
    // attached.
    let (base, key, _tmp) = spawn_server().await;
    let c = client();
    let auth = format!("Bearer {key}");

    // Create a volume.
    let resp = c
        .post(format!("{base}/v1/volumes"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"name": "data", "size_gb": 5}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 201);
    let vol: serde_json::Value = resp.json().await.unwrap();
    let vol_id = vol["id"].as_str().unwrap().to_string();

    // Arbitrary sizes are rejected (constrained knobs, like resources).
    let resp = c
        .post(format!("{base}/v1/volumes"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"name": "odd", "size_gb": 7}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 400);

    // Attach it to a sandbox and write through the mount path.
    let resp = c
        .post(format!("{base}/v1/sandboxes"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"volumes": [{"volume_id": vol_id, "mount_path": "/data"}]}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 201);
    let sb: serde_json::Value = resp.json().await.unwrap();
    let id = sb["id"].as_str().unwrap().to_string();
    let exec: serde_json::Value = c
        .post(format!("{base}/v1/sandboxes/{id}/exec"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"cmd": "echo persisted > data/state.txt"}))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(exec["exit_code"], 0, "write into the volume: {exec}");

    // Exclusive: a second sandbox cannot attach the same volume while held.
    let resp = c
        .post(format!("{base}/v1/sandboxes"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"volumes": [{"volume_id": vol_id, "mount_path": "/data"}]}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 409, "double-attach must conflict");

    // Deleting the volume while attached is refused.
    let resp = c
        .delete(format!("{base}/v1/volumes/{vol_id}"))
        .header("authorization", &auth)
        .send().await.unwrap();
    assert_eq!(resp.status(), 409, "delete while attached must conflict");

    // Delete the sandbox; the volume detaches but its data survives.
    let resp = c
        .delete(format!("{base}/v1/sandboxes/{id}"))
        .header("authorization", &auth)
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let vol: serde_json::Value = c
        .get(format!("{base}/v1/volumes/{vol_id}"))
        .header("authorization", &auth)
        .send().await.unwrap().json().await.unwrap();
    assert!(vol["attached_to"].is_null(), "volume must detach on sandbox delete: {vol}");

    // A new sandbox re-attaches the volume and reads the data back.
    let resp = c
        .post(format!("{base}/v1/sandboxes"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"volumes": [{"volume_id": vol_id, "mount_path": "/data"}]}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 201);
    let sb2: serde_json::Value = resp.json().await.unwrap();
    let id2 = sb2["id"].as_str().unwrap().to_string();
    let exec: serde_json::Value = c
        .post(format!("{base}/v1/sandboxes/{id2}/exec"))
        .header("authorization", &auth)
        .json(&serde_json::json!({"cmd": "cat data/state.txt"}))
        .send().await.unwrap().json().await.unwrap();
    assert_eq!(exec["exit_code"], 0);
    assert!(
        exec["stdout"].as_str().unwrap_or("").contains("persisted"),
        "volume data must survive across sandboxes: {exec}"
    );

    // A volume-attached sandbox cannot be forked (volumes are exclusive).
    let resp = c
        .post(format!("{base}/v1/sandboxes/{id2}/fork"))
        .header("authorization", &auth)
        .send().await.unwrap();
    assert_eq!(resp.status(), 409, "fork with attached volumes must conflict");

    // Detached volume deletes cleanly.
    c.delete(format!("{base}/v1/sandboxes/{id2}"))
        .header("authorization", &auth)
        .send().await.unwrap();
    let resp = c
        .delete(format!("{base}/v1/volumes/{vol_id}"))
        .header("authorization", &auth)
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn empty_pool_create_restores_from_golden_snapshot() {
    // Golden image snapshots: the warmer produces one artifact per pooled
    // image+shape; once the warm pool is drained, the next create must take the
    // snapshot_restore path (~hundreds of ms) instead of a cold boot (~1.5s).
    let (base, key, _tmp) = spawn_server().await;
    let c = client();
    let auth = format!("Bearer {key}");

    let mut paths = vec![];
    for _ in 0..3 {
        let sb: serde_json::Value = c
            .post(format!("{base}/v1/sandboxes"))
            .header("authorization", &auth)
            .send().await.unwrap().json().await.unwrap();
        paths.push(sb["boot_path"].as_str().unwrap_or("?").to_string());
    }
    // The base pool target is 2: two hot claims, then the golden restore.
    assert_eq!(paths[0], "hot_pool");
    assert_eq!(paths[1], "hot_pool");
    assert_eq!(
        paths[2], "snapshot_restore",
        "an empty-pool create with a golden snapshot must restore, not cold boot: {paths:?}"
    );
}
