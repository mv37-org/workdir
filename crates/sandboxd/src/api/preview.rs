//! Preview / VNC / CDP proxy (spec §16.2, §12). Routes `<id>-<port>` traffic to
//! the sandbox: plain HTTP via a forwarding client, and WebSocket upgrades
//! (noVNC, CDP) via a transparent message bridge. Authentication is required
//! unless a sandbox is explicitly public.

use crate::auth::{authenticate, AuthOutcome};
use crate::error::ApiError;
use crate::node::NodeClient;
use crate::state::AppState;
use axum::body::Body;
use axum::extract::ws::{Message as AxMsg, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::header::{HeaderName, HOST};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use std::collections::HashMap;
use tokio_tungstenite::tungstenite::Message as TgMsg;

#[derive(Deserialize)]
pub struct PreviewAuth {
    #[serde(default)]
    key: Option<String>,
}

/// Path-based preview for environments without wildcard DNS (dev/tests):
/// `/_preview/<id>/<port>/<rest>`.
pub async fn preview_path(
    State(state): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    req: axum::extract::Request,
) -> Response {
    let id = params.get("id").cloned().unwrap_or_default();
    let port: u16 = params.get("port").and_then(|p| p.parse().ok()).unwrap_or(0);
    let rest = params.get("rest").cloned().unwrap_or_default();
    // Drop the preview auth token before it is forwarded upstream (review H1).
    let qs = strip_key_query(req.uri().query());
    let upstream_path = format!("/{rest}{qs}");
    do_preview(state, id, port, upstream_path, req).await
}

/// Host-routed preview: `Host: <id>-<port>.<domain>`. Catches otherwise
/// unmatched routes.
pub async fn preview_host(State(state): State<AppState>, req: axum::extract::Request) -> Response {
    let host = req
        .headers()
        .get(HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();
    let label = host.split('.').next().unwrap_or("");
    let (id, port) = match parse_preview_label(label) {
        Some(v) => v,
        None => return ApiError::NotFound("route".into()).into_response(),
    };
    let path_only = req.uri().path();
    let qs = strip_key_query(req.uri().query());
    let upstream_path = format!("{path_only}{qs}");
    do_preview(state, id, port, upstream_path, req).await
}

fn parse_preview_label(label: &str) -> Option<(String, u16)> {
    // Hostname-safe `sbx-abcdef-3000` (or legacy `sbx_abcdef-3000`) -> ("sbx_abcdef", 3000).
    // `preview_url` renders the canonical id's `_` as `-` so each `<id>-<port>` host
    // is a valid DNS label (public ACME CAs reject `_`); map it back here. The hex
    // id body contains no `-`/`_`, so the leading `sbx-` is the only ambiguous one.
    let (id, port) = label.rsplit_once('-')?;
    let id = if let Some(rest) = id.strip_prefix("sbx-") {
        format!("sbx_{rest}")
    } else if id.starts_with("sbx_") {
        id.to_string()
    } else {
        return None;
    };
    Some((id, port.parse().ok()?))
}

/// Rebuild a query string with the `key=` preview token removed, so it is never
/// forwarded to the (untrusted) sandbox upstream or written to logs (review H1).
fn strip_key_query(query: Option<&str>) -> String {
    let q = match query {
        Some(q) if !q.is_empty() => q,
        _ => return String::new(),
    };
    let kept: Vec<&str> = q
        .split('&')
        .filter(|pair| {
            let name = pair.split('=').next().unwrap_or("");
            name != "key"
        })
        .collect();
    if kept.is_empty() {
        String::new()
    } else {
        format!("?{}", kept.join("&"))
    }
}

async fn do_preview(
    state: AppState,
    id: String,
    port: u16,
    upstream_path: String,
    req: axum::extract::Request,
) -> Response {
    // Authorize BEFORE leaking existence/state across orgs (review L2): an
    // unauthorized caller cannot distinguish "no such sandbox" from "not yours".
    let sb = state.store.get_sandbox(&id).ok().flatten();
    let authorized = sb
        .as_ref()
        .map(|s| preview_authorized(&state, &s.org_id, &req))
        .unwrap_or(false);
    let sb = match (sb, authorized) {
        (Some(s), true) => s,
        _ => return ApiError::NotFound(format!("sandbox {id}")).into_response(),
    };

    if !sb.state.is_active() {
        return (
            StatusCode::CONFLICT,
            format!("sandbox is {}", sb.state.as_str()),
        )
            .into_response();
    }

    // Only proxy ports the sandbox actually exposed, and never the control-plane
    // port — otherwise the preview becomes an SSRF gateway to the host /
    // control plane (review H2).
    if !sb.ports.contains(&port) {
        return (
            StatusCode::FORBIDDEN,
            format!("port {port} is not exposed by this sandbox"),
        )
            .into_response();
    }
    if port == control_plane_port(&state) {
        return (
            StatusCode::FORBIDDEN,
            "refusing to proxy to the control-plane port",
        )
            .into_response();
    }

    let handle = match &sb.runtime_handle {
        Some(h) => h.clone(),
        None => return ApiError::Conflict("no runtime handle".into()).into_response(),
    };
    let upstream = match state.local.expose_port(&handle, port).await {
        Ok(addr) => addr,
        Err(e) => {
            return (StatusCode::BAD_GATEWAY, format!("expose_port failed: {e}")).into_response()
        }
    };

    // Activity through the preview/VNC channel keeps the sandbox alive
    // (review #6): refresh before forwarding.
    if let Ok(Some(mut fresh)) = state.store.get_sandbox(&id) {
        crate::service::touch_activity(&state, &mut fresh);
    }

    // WebSocket upgrade (noVNC / CDP)?
    let (mut parts, body) = req.into_parts();
    if let Ok(ws) = WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        let ws_url = format!("ws://{}{}", upstream, upstream_path);
        return ws.on_upgrade(move |client| bridge_ws(client, ws_url));
    }

    // Plain HTTP forward.
    let method = parts.method.clone();
    let headers = parts.headers.clone();
    let body_bytes = match axum::body::to_bytes(body, 16 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "body too large").into_response(),
    };
    http_forward(
        &state,
        method,
        &headers,
        upstream,
        &upstream_path,
        body_bytes.to_vec(),
    )
    .await
}

/// The port the control plane itself listens on (parsed from the bind address),
/// which must never be a preview upstream target.
fn control_plane_port(state: &AppState) -> u16 {
    state
        .cfg
        .server
        .bind
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(0)
}

fn preview_authorized(state: &AppState, org_id: &str, req: &axum::extract::Request) -> bool {
    // Bearer header or ?key= query, must belong to the sandbox org (or admin).
    let bearer = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    let query_key = Query::<PreviewAuth>::try_from_uri(req.uri())
        .ok()
        .and_then(|q| q.0.key);
    let token = bearer.or(query_key);
    match authenticate(&state.store, token.as_deref()) {
        AuthOutcome::Ok(ctx) => ctx.admin || ctx.org_id == org_id,
        _ => false,
    }
}

async fn http_forward(
    state: &AppState,
    method: Method,
    headers: &HeaderMap,
    upstream: std::net::SocketAddr,
    path: &str,
    body: Vec<u8>,
) -> Response {
    let url = format!("http://{upstream}{path}");
    let mut builder = state.http.request(method, &url);
    for (name, value) in headers.iter() {
        if is_hop_by_hop(name) || name == HOST {
            continue;
        }
        builder = builder.header(name, value);
    }
    let resp = match builder.body(body).send().await {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("upstream error: {e}")).into_response(),
    };
    let status = resp.status();
    let mut out = Response::builder().status(status);
    for (name, value) in resp.headers().iter() {
        if is_hop_by_hop(name) {
            continue;
        }
        out = out.header(name, value);
    }
    let bytes = resp.bytes().await.unwrap_or_default();
    out.body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
            | "content-length"
    )
}

/// Bridge a client WebSocket to an upstream WebSocket (noVNC, CDP). Metered
/// per spec §12.2 (the byte counts would feed VNC/noVNC bandwidth accounting).
async fn bridge_ws(client: WebSocket, upstream_url: String) {
    let upstream = match tokio_tungstenite::connect_async(&upstream_url).await {
        Ok((s, _)) => s,
        Err(e) => {
            // Log only the host:port, never the full URL (it may carry query
            // params / tokens) (review H1).
            let redacted = upstream_url.split('?').next().unwrap_or("ws://?");
            tracing::warn!(error = %e, upstream = %redacted, "upstream ws connect failed");
            return;
        }
    };
    let (mut up_tx, mut up_rx) = upstream.split();
    let (mut cl_tx, mut cl_rx) = client.split();

    let client_to_upstream = async {
        while let Some(Ok(msg)) = cl_rx.next().await {
            let tg = match msg {
                AxMsg::Text(t) => TgMsg::Text(t),
                AxMsg::Binary(b) => TgMsg::Binary(b),
                AxMsg::Ping(p) => TgMsg::Ping(p),
                AxMsg::Pong(p) => TgMsg::Pong(p),
                AxMsg::Close(_) => break,
            };
            if up_tx.send(tg).await.is_err() {
                break;
            }
        }
    };
    let upstream_to_client = async {
        while let Some(Ok(msg)) = up_rx.next().await {
            let ax = match msg {
                TgMsg::Text(t) => AxMsg::Text(t),
                TgMsg::Binary(b) => AxMsg::Binary(b),
                TgMsg::Ping(p) => AxMsg::Ping(p),
                TgMsg::Pong(p) => AxMsg::Pong(p),
                TgMsg::Close(_) => break,
                TgMsg::Frame(_) => continue,
            };
            if cl_tx.send(ax).await.is_err() {
                break;
            }
        }
    };

    tokio::select! {
        _ = client_to_upstream => {},
        _ = upstream_to_client => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_label_maps_hostname_safe_id_back_to_canonical() {
        // Hostname-safe form (what `preview_url` now emits): `_` rendered as `-`.
        assert_eq!(
            parse_preview_label("sbx-ab12cd34ef56-3000"),
            Some(("sbx_ab12cd34ef56".to_string(), 3000))
        );
        // Legacy underscore form still parses (path tools / older URLs).
        assert_eq!(
            parse_preview_label("sbx_ab12cd34ef56-8080"),
            Some(("sbx_ab12cd34ef56".to_string(), 8080))
        );
        // VNC/CDP ports.
        assert_eq!(
            parse_preview_label("sbx-deadbeef0000-6080"),
            Some(("sbx_deadbeef0000".to_string(), 6080))
        );
        // Non-sandbox labels (e.g. the `api` host) are not previews.
        assert_eq!(parse_preview_label("api"), None);
        assert_eq!(parse_preview_label("foo-3000"), None);
    }
}
