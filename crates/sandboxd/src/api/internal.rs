//! Worker-side internal node API. The control plane drives a worker's data
//! plane through these endpoints (see [`crate::remote::RemoteNodeClient`]).
//!
//! Mounted at `/internal`, authenticated by the shared cluster secret
//! (`node.rpc_token`) in the `X-Node-Token` header. NOT part of the public
//! `/v1` surface — it is the control-plane↔worker control channel.

use crate::node::NodeClient;
use crate::remote::{b64, unb64, NODE_TOKEN_HEADER};
use crate::runtime::{ExecRequest, VmSpec};
use crate::state::AppState;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

type R = Result<Json<Value>, (StatusCode, String)>;
fn err(e: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::BAD_GATEWAY, e.to_string())
}

fn node_err(e: anyhow::Error) -> (StatusCode, String) {
    if crate::node::is_file_not_found(&e) {
        (StatusCode::NOT_FOUND, "not_found".to_string())
    } else {
        err(e)
    }
}

pub fn router(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/place", post(place))
        .route("/exec", post(exec))
        .route("/read_file", post(read_file))
        .route("/write_file", post(write_file))
        .route("/list_dir", post(list_dir))
        .route("/expose_port", post(expose_port))
        .route("/ready_check", post(ready_check))
        .route("/pause", post(pause))
        .route("/resume", post(resume))
        .route("/standby", post(standby))
        .route("/restore", post(restore))
        .route("/snapshot", post(snapshot))
        .route("/fork", post(fork))
        .route("/delete", post(delete))
        .route("/hot_pool_available", post(hot_pool_available))
        // State for routes is provided by the outer router's `.with_state`.
        .layer(middleware::from_fn_with_state(state, node_auth))
}

/// Validate the shared cluster secret. Disabled (always 503) if no token is
/// configured, so the internal API is never open by default.
async fn node_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let expected = state.cfg.node.rpc_token.trim();
    if expected.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "internal node API disabled (no rpc_token)",
        )
            .into_response();
    }
    let presented = req
        .headers()
        .get(NODE_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    // length-checked constant-ish compare
    if presented.len() != expected.len()
        || presented
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |a, (x, y)| a | (x ^ y))
            != 0
    {
        return (StatusCode::UNAUTHORIZED, "bad node token").into_response();
    }
    next.run(req).await
}

#[derive(Deserialize)]
struct PlaceReq {
    spec: VmSpec,
    snapshot_available: bool,
}
async fn place(State(s): State<AppState>, Json(r): Json<PlaceReq>) -> R {
    let inst = s
        .local
        .place(&r.spec, r.snapshot_available)
        .await
        .map_err(err)?;
    Ok(Json(serde_json::to_value(inst).unwrap()))
}

#[derive(Deserialize)]
struct ExecReq {
    handle: String,
    req: ExecRequest,
}
async fn exec(State(s): State<AppState>, Json(r): Json<ExecReq>) -> R {
    let res = s.local.exec(&r.handle, &r.req).await.map_err(err)?;
    Ok(Json(serde_json::to_value(res).unwrap()))
}

#[derive(Deserialize)]
struct PathReq {
    handle: String,
    path: String,
}
async fn read_file(State(s): State<AppState>, Json(r): Json<PathReq>) -> R {
    let bytes = s
        .local
        .read_file(&r.handle, &r.path)
        .await
        .map_err(node_err)?;
    Ok(Json(json!({ "data_b64": b64(&bytes) })))
}

#[derive(Deserialize)]
struct WriteReq {
    handle: String,
    path: String,
    data_b64: String,
}
async fn write_file(State(s): State<AppState>, Json(r): Json<WriteReq>) -> R {
    let bytes = unb64(&r.data_b64).map_err(err)?;
    s.local
        .write_file(&r.handle, &r.path, &bytes)
        .await
        .map_err(err)?;
    Ok(Json(json!({})))
}

async fn list_dir(State(s): State<AppState>, Json(r): Json<PathReq>) -> R {
    let entries = s.local.list_dir(&r.handle, &r.path).await.map_err(err)?;
    Ok(Json(json!({ "entries": entries })))
}

#[derive(Deserialize)]
struct PortReq {
    handle: String,
    port: u16,
}
async fn expose_port(State(s): State<AppState>, Json(r): Json<PortReq>) -> R {
    let addr = s.local.expose_port(&r.handle, r.port).await.map_err(err)?;
    Ok(Json(json!({ "addr": addr.to_string() })))
}

#[derive(Deserialize)]
struct ReadyReq {
    handle: String,
    url: String,
    timeout_seconds: u32,
}
async fn ready_check(State(s): State<AppState>, Json(r): Json<ReadyReq>) -> R {
    s.local
        .ready_check(&r.handle, &r.url, r.timeout_seconds)
        .await
        .map_err(err)?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct PauseReq {
    handle: String,
    persist: bool,
}
async fn pause(State(s): State<AppState>, Json(r): Json<PauseReq>) -> R {
    s.local.pause(&r.handle, r.persist).await.map_err(err)?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct HandleReq {
    handle: String,
}
async fn resume(State(s): State<AppState>, Json(r): Json<HandleReq>) -> R {
    let ms = s.local.resume(&r.handle).await.map_err(err)?;
    Ok(Json(json!({ "resume_ms": ms })))
}
async fn standby(State(s): State<AppState>, Json(r): Json<HandleReq>) -> R {
    let ms = s.local.standby(&r.handle).await.map_err(err)?;
    Ok(Json(json!({ "standby_ms": ms })))
}
async fn restore(State(s): State<AppState>, Json(r): Json<HandleReq>) -> R {
    let ms = s.local.restore(&r.handle).await.map_err(err)?;
    Ok(Json(json!({ "restore_ms": ms })))
}
async fn snapshot(State(s): State<AppState>, Json(r): Json<HandleReq>) -> R {
    let art = s.local.snapshot(&r.handle).await.map_err(err)?;
    Ok(Json(serde_json::to_value(art).unwrap()))
}
#[derive(Deserialize)]
struct ForkReq {
    parent_handle: String,
    spec: VmSpec,
}
async fn fork(State(s): State<AppState>, Json(r): Json<ForkReq>) -> R {
    let inst = s.local.fork(&r.parent_handle, &r.spec).await.map_err(err)?;
    Ok(Json(serde_json::to_value(inst).unwrap()))
}
async fn delete(State(s): State<AppState>, Json(r): Json<HandleReq>) -> R {
    s.local.delete(&r.handle).await.map_err(err)?;
    Ok(Json(json!({})))
}

#[derive(Deserialize)]
struct PoolReq {
    image_key: String,
    resources: crate::knobs::Resources,
}
async fn hot_pool_available(State(s): State<AppState>, Json(r): Json<PoolReq>) -> R {
    let count = s.local.hot_pool_available(&r.image_key, &r.resources).await;
    Ok(Json(json!({ "count": count })))
}
