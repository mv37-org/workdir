//! HTTP API surface (spec §19) built on axum. The `/v1` routes are gated by an
//! API-key auth middleware; preview/VNC traffic is served by a host-routed
//! fallback (spec §16.2).

pub mod admin;
pub mod images;
pub mod internal;
pub mod nodes;
pub mod preview;
pub mod pty;
pub mod sandboxes;
pub mod secrets;
pub mod usage;
pub mod volumes;

use crate::auth::{authenticate, AuthContext, AuthOutcome};
use crate::error::ApiError;
use crate::model::Sandbox;
use crate::state::AppState;
use axum::extract::{Request, State};
use axum::http::header::AUTHORIZATION;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde_json::json;

pub fn router(state: AppState) -> Router {
    let v1 = Router::new()
        .route("/sandboxes", post(sandboxes::create).get(sandboxes::list))
        .route(
            "/sandboxes/:id",
            get(sandboxes::get).delete(sandboxes::delete),
        )
        .route("/sandboxes/:id/exec", post(sandboxes::exec))
        .route("/sandboxes/:id/exec/:cmd_id", get(sandboxes::exec_status))
        .route(
            "/sandboxes/:id/exec/:cmd_id/logs",
            get(sandboxes::exec_logs),
        )
        .route("/sandboxes/:id/metrics", get(sandboxes::metrics))
        .route("/sandboxes/:id/pty", get(pty::pty_ws))
        .route(
            "/sandboxes/:id/files",
            get(sandboxes::read_file).put(sandboxes::write_file),
        )
        .route(
            "/sandboxes/:id/ports/:port/expose",
            post(sandboxes::expose_port),
        )
        .route(
            "/sandboxes/:id/browser",
            post(sandboxes::browser_get).get(sandboxes::browser_get),
        )
        .route(
            "/sandboxes/:id/browser/screenshot",
            get(sandboxes::browser_screenshot),
        )
        .route("/sandboxes/:id/snapshot", post(sandboxes::snapshot))
        .route("/sandboxes/:id/fork", post(sandboxes::fork))
        .route("/sandboxes/:id/pause", post(sandboxes::pause))
        .route("/sandboxes/:id/resume", post(sandboxes::resume))
        .route("/images", get(images::list).post(images::create))
        .route("/images/:id", get(images::get).delete(images::delete))
        .route("/nodes", get(nodes::list))
        .route("/nodes/join-token", post(nodes::join_token))
        .route("/nodes/:id/drain", post(nodes::drain))
        .route("/secrets", get(secrets::list))
        .route("/secrets/:name", put(secrets::put).delete(secrets::delete))
        .route("/volumes", get(volumes::list).post(volumes::create))
        .route("/volumes/:id", get(volumes::get).delete(volumes::delete))
        .route("/usage", get(usage::usage))
        .route("/admin/overview", get(usage::admin_overview))
        .route("/admin/metrics", get(admin::metrics))
        .route("/admin/orgs", post(admin::create_org))
        .route("/admin/orgs/:org/usage", get(admin::org_usage))
        .route("/admin/orgs/:org/suspend", post(admin::org_suspend))
        .route("/admin/orgs/:org/unsuspend", post(admin::org_unsuspend))
        .route("/admin/orgs/:org/secrets", get(admin::org_secrets_list))
        .route(
            "/admin/orgs/:org/secrets/:name",
            put(admin::org_secret_put).delete(admin::org_secret_delete),
        )
        .route("/admin/orgs/:org/sandboxes", get(admin::org_sandboxes))
        .route(
            "/admin/orgs/:org/sandboxes/:id/stop",
            post(admin::org_sandbox_stop),
        )
        .route(
            "/admin/orgs/:org/sandboxes/:id",
            delete(admin::org_sandbox_delete),
        )
        .route("/admin/orgs/:org/images", get(admin::org_images))
        .route(
            "/admin/orgs/:org/images/:id",
            delete(admin::org_image_delete),
        )
        .route("/admin/keys", post(admin::register_key))
        .route("/admin/keys/:hash", delete(admin::revoke_key))
        .route("/benchmarks", get(usage::benchmarks))
        .route("/benchmarks/run", post(usage::run_benchmarks))
        .layer(middleware::from_fn_with_state(state.clone(), auth_mw));

    Router::new()
        .route("/healthz", get(healthz))
        .nest("/v1", v1)
        // Control-plane↔worker RPC (token-authed; disabled without node.rpc_token).
        .nest("/internal", internal::router(state.clone()))
        // Path-based preview for environments without wildcard DNS (dev/tests).
        .route(
            "/_preview/:id/:port",
            get(preview::preview_path).post(preview::preview_path),
        )
        .route(
            "/_preview/:id/:port/*rest",
            get(preview::preview_path).post(preview::preview_path),
        )
        // Host-routed preview (`<id>-<port>.domain`) lands here.
        .fallback(preview::preview_host)
        .with_state(state)
}

async fn healthz() -> impl IntoResponse {
    Json(json!({"status": "ok"}))
}

/// API-key auth middleware (spec §6.1, §18). On success, inserts an
/// [`AuthContext`] into request extensions for handlers to read.
async fn auth_mw(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let bearer = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|s| s.to_string());
    match authenticate(&state.store, bearer.as_deref()) {
        AuthOutcome::Ok(ctx) => {
            req.extensions_mut().insert(ctx);
            next.run(req).await
        }
        AuthOutcome::Suspended => ApiError::Forbidden("org suspended".into()).into_response(),
        AuthOutcome::Missing | AuthOutcome::Invalid => ApiError::Unauthorized.into_response(),
    }
}

/// Load a sandbox and enforce ownership. Returns NotFound (not Forbidden) for
/// other orgs' sandboxes so existence is not leaked.
pub fn load_owned(state: &AppState, ctx: &AuthContext, id: &str) -> Result<Sandbox, ApiError> {
    let sb = state
        .store
        .get_sandbox(id)
        .map_err(ApiError::Internal)?
        .ok_or_else(|| ApiError::NotFound(format!("sandbox {id}")))?;
    if sb.org_id != ctx.org_id && !ctx.admin {
        return Err(ApiError::NotFound(format!("sandbox {id}")));
    }
    Ok(sb)
}
