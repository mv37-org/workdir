//! Sandbox endpoints (spec §19).

use crate::api::load_owned;
use crate::auth::AuthContext;
use crate::error::{ApiError, ApiResult};
use crate::model::CreateSandboxRequest;
use crate::runtime::ExecRequest;
use crate::service;
use crate::state::AppState;
use crate::views::sandbox_view;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub async fn create(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    body: Option<Json<CreateSandboxRequest>>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    // Body is optional so `client.sandboxes.create()` (no body) works.
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let sb = service::create_sandbox(&state, &ctx, req).await?;
    Ok((StatusCode::CREATED, Json(sandbox_view(&state, &sb))))
}

pub async fn list(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    let sandboxes = state
        .store
        .list_sandboxes_for_org(&ctx.org_id)
        .map_err(ApiError::Internal)?;
    let views: Vec<Value> = sandboxes.iter().map(|s| sandbox_view(&state, s)).collect();
    Ok(Json(json!({ "sandboxes": views })))
}

pub async fn get(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let sb = load_owned(&state, &ctx, &id)?;
    Ok(Json(sandbox_view(&state, &sb)))
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let sb = load_owned(&state, &ctx, &id)?;
    service::delete_sandbox(&state, sb).await?;
    Ok(Json(json!({ "id": id, "deleted": true })))
}

#[derive(Deserialize)]
pub struct ExecBody {
    pub cmd: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub background: bool,
}

pub async fn exec(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
    Json(body): Json<ExecBody>,
) -> ApiResult<Json<Value>> {
    let sb = load_owned(&state, &ctx, &id)?;
    // Perpetual standby: a parked sandbox transparently auto-resumes here, so a
    // client that hasn't touched it in a while just sees a slightly slower exec.
    let mut sb = service::ensure_running(&state, sb).await?;
    if !sb.state.is_active() {
        return Err(ApiError::Conflict(format!("sandbox is {}", sb.state.as_str())));
    }
    let handle = sb.runtime_handle.clone().ok_or_else(|| ApiError::Conflict("no runtime handle".into()))?;
    // Mark activity before and after so neither the run nor a quick follow-up is
    // mistaken for idle (review #7).
    service::touch_activity(&state, &mut sb);
    let result = state
        .node_for(sb.node_id.as_deref().unwrap_or(""))
        .exec(
            &handle,
            &ExecRequest { cmd: body.cmd, cwd: body.cwd, env: body.env, background: body.background },
        )
        .await
        .map_err(ApiError::Internal)?;
    service::touch_activity(&state, &mut sb);
    Ok(Json(json!({
        "exit_code": result.exit_code,
        "stdout": result.stdout,
        "stderr": result.stderr,
    })))
}

#[derive(Deserialize)]
pub struct FileQuery {
    pub path: String,
}

pub async fn read_file(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
    Query(q): Query<FileQuery>,
) -> ApiResult<Json<Value>> {
    let sb = load_owned(&state, &ctx, &id)?;
    let sb = service::ensure_running(&state, sb).await?;
    let handle = sb.runtime_handle.clone().ok_or_else(|| ApiError::Conflict("no runtime handle".into()))?;
    let bytes = state.node_for(sb.node_id.as_deref().unwrap_or("")).read_file(&handle, &q.path).await.map_err(ApiError::Internal)?;
    let body = match String::from_utf8(bytes.clone()) {
        Ok(text) => json!({ "path": q.path, "encoding": "utf8", "content": text }),
        Err(_) => json!({ "path": q.path, "encoding": "base64", "content": base64(&bytes) }),
    };
    Ok(Json(body))
}

#[derive(Deserialize)]
pub struct WriteFileBody {
    pub path: String,
    pub content: String,
    /// "utf8" (default) or "base64".
    #[serde(default)]
    pub encoding: Option<String>,
}

pub async fn write_file(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
    Json(body): Json<WriteFileBody>,
) -> ApiResult<Json<Value>> {
    let sb = load_owned(&state, &ctx, &id)?;
    let sb = service::ensure_running(&state, sb).await?;
    let handle = sb.runtime_handle.clone().ok_or_else(|| ApiError::Conflict("no runtime handle".into()))?;
    let bytes = match body.encoding.as_deref() {
        Some("base64") => unbase64(&body.content).map_err(ApiError::BadRequest)?,
        _ => body.content.into_bytes(),
    };
    let len = bytes.len();
    state.node_for(sb.node_id.as_deref().unwrap_or("")).write_file(&handle, &body.path, &bytes).await.map_err(ApiError::Internal)?;
    Ok(Json(json!({ "path": body.path, "written": true, "bytes": len })))
}

pub async fn expose_port(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path((id, port)): Path<(String, u16)>,
) -> ApiResult<Json<Value>> {
    let sb = load_owned(&state, &ctx, &id)?;
    let mut sb = service::ensure_running(&state, sb).await?;
    let handle = sb.runtime_handle.clone().ok_or_else(|| ApiError::Conflict("no runtime handle".into()))?;
    state.node_for(sb.node_id.as_deref().unwrap_or("")).expose_port(&handle, port).await.map_err(ApiError::Internal)?;
    if !sb.ports.contains(&port) {
        sb.ports.push(port);
        sb.updated_at = chrono::Utc::now();
        state.store.put_sandbox(&sb).map_err(ApiError::Internal)?;
    }
    Ok(Json(json!({
        "port": port,
        "url": state.preview_url(&sb.id, port),
    })))
}

pub async fn browser_get(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let sb = load_owned(&state, &ctx, &id)?;
    if !sb.browser_enabled() {
        return Err(ApiError::BadRequest("browser is not enabled on this sandbox".into()));
    }
    Ok(Json(json!({
        "enabled": true,
        "ready": sb.timings.browser_ready_ms > 0,
        "browser_ready_ms": sb.timings.browser_ready_ms,
        "urls": {
            "vnc": state.preview_url(&sb.id, 6080),
            "cdp": state.preview_url(&sb.id, 9222),
            "screenshot": format!("/v1/sandboxes/{}/browser/screenshot", sb.id),
        }
    })))
}

/// Capture a PNG of the browser sandbox's **live desktop** (what the VNC URL
/// shows) by grabbing the X root window with ImageMagick, then reading the file
/// back. Advertised by `browser_get`. Capturing the X display works regardless
/// of whether chrome's CDP port is up, and shows the whole desktop, not just a
/// page viewport.
pub async fn browser_screenshot(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let sb = match load_owned(&state, &ctx, &id) {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if !sb.browser_enabled() {
        return ApiError::BadRequest("browser is not enabled on this sandbox".into()).into_response();
    }
    // A parked browser sandbox transparently auto-resumes, same as exec.
    let mut sb = match service::ensure_running(&state, sb).await {
        Ok(s) => s,
        Err(e) => return e.into_response(),
    };
    if !sb.state.is_active() {
        return ApiError::Conflict(format!("sandbox is {}", sb.state.as_str())).into_response();
    }
    let handle = match sb.runtime_handle.clone() {
        Some(h) => h,
        None => return ApiError::Conflict("no runtime handle".into()).into_response(),
    };
    service::touch_activity(&state, &mut sb);
    let node = state.node_for(sb.node_id.as_deref().unwrap_or(""));

    let shot = "/tmp/wd-screenshot.png";
    let cmd = format!("rm -f {shot}; DISPLAY=:0 import -window root {shot} 2>/dev/null; test -s {shot}");
    if let Err(e) = node
        .exec(&handle, &ExecRequest { cmd, cwd: None, env: Default::default(), background: false })
        .await
    {
        return (StatusCode::BAD_GATEWAY, format!("capture exec failed: {e}")).into_response();
    }
    match node.read_file(&handle, shot).await {
        Ok(png) if !png.is_empty() => {
            ([(axum::http::header::CONTENT_TYPE, "image/png")], png).into_response()
        }
        Ok(_) => (StatusCode::BAD_GATEWAY, "screenshot was empty (is the desktop up?)").into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("screenshot read failed: {e}")).into_response(),
    }
}

pub async fn snapshot(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let sb = load_owned(&state, &ctx, &id)?;
    let snap = service::snapshot_sandbox(&state, &sb).await?;
    Ok(Json(json!({
        "id": snap.id,
        "sandbox_id": snap.sandbox_id,
        "storage_bytes": snap.storage_bytes,
        "created_at": snap.created_at,
    })))
}

pub async fn fork(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let parent = load_owned(&state, &ctx, &id)?;
    let child = service::fork_sandbox(&state, &ctx, parent).await?;
    Ok((StatusCode::CREATED, Json(sandbox_view(&state, &child))))
}

pub async fn pause(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let sb = load_owned(&state, &ctx, &id)?;
    let sb = service::stop_sandbox(&state, sb).await?;
    Ok(Json(sandbox_view(&state, &sb)))
}

pub async fn resume(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let sb = load_owned(&state, &ctx, &id)?;
    let sb = service::resume_sandbox(&state, sb).await?;
    Ok(Json(sandbox_view(&state, &sb)))
}

// --- base64 helpers (shared alphabet with the guest agent) ------------------

const B64: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn base64(input: &[u8]) -> String {
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        out.push(B64[(b[0] >> 2) as usize] as char);
        out.push(B64[(((b[0] & 0x03) << 4) | (b[1] >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 { B64[(((b[1] & 0x0f) << 2) | (b[2] >> 6)) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[(b[2] & 0x3f) as usize] as char } else { '=' });
    }
    out
}

fn unbase64(input: &str) -> Result<Vec<u8>, String> {
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
