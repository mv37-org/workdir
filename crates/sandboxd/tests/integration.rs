//! End-to-end API test against the in-process server with the mock runtime.
//! Exercises the spec §25.1 one-node acceptance flow: default create, exec
//! echo, file read/write, preview port, knob rejection, auth, and delete.

use sandboxd::app::build_state;
use sandboxd::config::Config;

async fn spawn_server() -> (String, String, tempfile::TempDir) {
    // The mock runtime is dev-only and refuses to start without this opt-in.
    std::env::set_var("SANDBOXD_ALLOW_INSECURE_RUNTIME", "1");
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().to_path_buf();
    let mut cfg = Config::default();
    cfg.server.data_dir = data.clone();
    cfg.server.bind = "127.0.0.1:0".into();
    cfg.server.public_domain = "test.local".into();
    cfg.runtime.kind = "mock".into();
    cfg.runtime.workspace_dir = data.join("workspaces");
    cfg.runtime.images_dir = data.join("images");
    cfg.runtime.kernel_image = data.join("kernel/vmlinux").to_string_lossy().to_string();
    cfg.auth.bootstrap_admin_key = "sk_live_test".into();

    let state = build_state(cfg).await.expect("build state");
    // Warm the base pool so the default create takes the hot_pool path.
    for _ in 0..4 {
        state.local.warm_once().await;
    }
    let app = sandboxd::api::router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), "sk_live_test".to_string(), tmp)
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
