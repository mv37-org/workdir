//! Image endpoints (spec §10, §11). Curated images are listed statically;
//! custom images are built/imported asynchronously and never on the create path.

use crate::auth::AuthContext;
use crate::catalog::curated_images;
use crate::error::{ApiError, ApiResult};
use crate::ids;
use crate::images::*;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::Utc;
use serde_json::{json, Value};

pub async fn list(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
) -> ApiResult<Json<Value>> {
    let curated: Vec<Value> = curated_images()
        .into_iter()
        .map(|c| {
            json!({
                "id": c.id,
                "kind": c.kind,
                "intended_use": c.intended_use,
                "min_resources": { "cpu": c.min_cpu, "memory_mb": c.min_memory_mb, "disk_gb": c.min_disk_gb },
                "hot_pool_priority": c.hot_pool_priority,
                "immutable": c.immutable,
            })
        })
        .collect();
    let custom = state.store.list_images_for_org(&ctx.org_id).map_err(ApiError::Internal)?;
    let custom_views: Vec<Value> = custom.iter().map(image_view).collect();
    Ok(Json(json!({ "curated": curated, "custom": custom_views })))
}

pub async fn create(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Json(req): Json<CreateImageRequest>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if !req.name.starts_with("custom/") {
        return Err(ApiError::BadRequest("image name must be 'custom/<org>/<name>'".into()));
    }
    let source = match req.source.source_type.as_str() {
        "dockerfile" => {
            let ctx_url = req
                .source
                .context_url
                .clone()
                .ok_or_else(|| ApiError::BadRequest("dockerfile source needs context_url".into()))?;
            ImageSource::Dockerfile {
                context_url: ctx_url,
                dockerfile: req.source.dockerfile.clone().unwrap_or_else(|| "Dockerfile".into()),
            }
        }
        "oci" => {
            let r = req
                .source
                .image_ref
                .clone()
                .ok_or_else(|| ApiError::BadRequest("oci source needs image_ref".into()))?;
            ImageSource::Oci { image_ref: r }
        }
        other => return Err(ApiError::BadRequest(format!("unknown source type '{other}'"))),
    };

    let now = Utc::now();
    let version = format!("{}-{}", now.format("%Y-%m-%d"), &ids::image_id()[4..10]);
    let expires_at = if req.ephemeral {
        let ttl = req.ttl_seconds.unwrap_or(3600);
        Some(now + chrono::Duration::seconds(ttl as i64))
    } else {
        None
    };
    let img = CustomImage {
        id: ids::image_id(),
        org_id: ctx.org_id.clone(),
        name: req.name.clone(),
        version,
        source,
        status: ImageStatus::Building,
        resources_hint: req.resources_hint.clone(),
        build_log: "queued\n".into(),
        first_node_cache_miss_ms: None,
        storage_bytes: 0,
        ephemeral: req.ephemeral,
        expires_at,
        created_at: now,
        updated_at: now,
    };
    state.store.put_image(&img).map_err(ApiError::Internal)?;

    // Build asynchronously (spec §11: never build/pull synchronously on create).
    spawn_build(state.clone(), img.id.clone());

    Ok((StatusCode::ACCEPTED, Json(image_view(&img))))
}

pub async fn get(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let img = state
        .store
        .get_image(&id)
        .map_err(ApiError::Internal)?
        .filter(|i| i.org_id == ctx.org_id || ctx.admin)
        .ok_or_else(|| ApiError::NotFound(format!("image {id}")))?;
    Ok(Json(image_view(&img)))
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(ctx): Extension<AuthContext>,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let mut img = state
        .store
        .get_image(&id)
        .map_err(ApiError::Internal)?
        .filter(|i| i.org_id == ctx.org_id || ctx.admin)
        .ok_or_else(|| ApiError::NotFound(format!("image {id}")))?;
    // Soft delete: prevents new creates, does not kill running sandboxes (§25.3).
    img.status = ImageStatus::Deleted;
    img.updated_at = Utc::now();
    state.store.put_image(&img).map_err(ApiError::Internal)?;
    Ok(Json(json!({ "id": id, "deleted": true })))
}

fn image_view(img: &CustomImage) -> Value {
    json!({
        "id": img.id,
        "name": img.name,
        "version": img.version,
        "reference": img.reference(),
        "status": img.status.as_str(),
        "build_log": img.build_log,
        "first_node_cache_miss_ms": img.first_node_cache_miss_ms,
        "storage_bytes": img.storage_bytes,
        "ephemeral": img.ephemeral,
        "expires_at": img.expires_at,
        "created_at": img.created_at,
        "updated_at": img.updated_at,
    })
}

/// Mock image builder. The production builder runs the §10.3/§11 pipeline:
/// build rootfs deterministically, inject + validate the guest agent, boot,
/// health-check, scrub secrets, snapshot, and publish an immutable version.
fn spawn_build(state: AppState, image_id: String) {
    tokio::spawn(async move {
        let steps = [
            ("fetching build context", 150u64),
            ("building rootfs", 600),
            ("injecting guest agent", 120),
            ("validating guest agent", 120),
            ("scanning for blocked capabilities", 200),
            ("booting + health check", 400),
            ("scrubbing secrets and machine-id", 100),
            ("publishing immutable version", 120),
        ];
        let mut log = String::new();
        let mut total_ms = 0u64;
        for (msg, ms) in steps {
            tokio::time::sleep(std::time::Duration::from_millis(ms / 4)).await; // scaled for dev
            total_ms += ms;
            log.push_str(&format!("[{total_ms:>5}ms] {msg}\n"));
            if let Ok(Some(mut img)) = state.store.get_image(&image_id) {
                img.build_log = log.clone();
                img.updated_at = Utc::now();
                state.store.put_image(&img).ok();
            }
        }
        if let Ok(Some(mut img)) = state.store.get_image(&image_id) {
            img.status = ImageStatus::Ready;
            img.first_node_cache_miss_ms = Some(total_ms);
            img.storage_bytes = 480 * 1024 * 1024; // ~480 MB placeholder artifact
            img.build_log = format!("{log}published {}\n", img.reference());
            img.updated_at = Utc::now();
            state.store.put_image(&img).ok();
            tracing::info!(image = %img.reference(), "custom image published");
        }
    });
}
