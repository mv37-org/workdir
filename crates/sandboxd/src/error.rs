//! Error types and HTTP mapping.
//!
//! Every rejection carries a stable `code` so clients (and the scheduler, per
//! spec §15 "expose why a request was rejected") can branch on it.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("{0} not found")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    /// Scheduler could not place the sandbox. `reason` is a stable machine code.
    #[error("no capacity: {reason}")]
    NoCapacity { reason: String, detail: String },
    #[error("rejected: {0}")]
    Rejected(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl ApiError {
    pub fn code(&self) -> &'static str {
        match self {
            ApiError::BadRequest(_) => "bad_request",
            ApiError::Unauthorized => "unauthorized",
            ApiError::Forbidden(_) => "forbidden",
            ApiError::NotFound(_) => "not_found",
            ApiError::Conflict(_) => "conflict",
            ApiError::NoCapacity { .. } => "no_capacity",
            ApiError::Rejected(_) => "rejected",
            ApiError::Internal(_) => "internal",
        }
    }

    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::BadRequest(_) => StatusCode::BAD_REQUEST,
            ApiError::Unauthorized => StatusCode::UNAUTHORIZED,
            ApiError::Forbidden(_) => StatusCode::FORBIDDEN,
            ApiError::NotFound(_) => StatusCode::NOT_FOUND,
            ApiError::Conflict(_) => StatusCode::CONFLICT,
            // 503: capacity is a transient, retryable condition.
            ApiError::NoCapacity { .. } => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::Rejected(_) => StatusCode::UNPROCESSABLE_ENTITY,
            ApiError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorInner,
}

#[derive(Serialize)]
struct ErrorInner {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let ApiError::Internal(e) = &self {
            tracing::error!(error = ?e, "internal error");
        }
        let reason = match &self {
            ApiError::NoCapacity { reason, .. } => Some(reason.clone()),
            _ => None,
        };
        let message = match &self {
            ApiError::NoCapacity { detail, .. } => detail.clone(),
            // Do not leak internal error chains to clients.
            ApiError::Internal(_) => "internal server error".to_string(),
            other => other.to_string(),
        };
        let body = ErrorBody {
            error: ErrorInner { code: self.code(), message, reason },
        };
        (self.status(), Json(body)).into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

/// Convenience: turn any error context into a BadRequest.
pub fn bad_request(msg: impl Into<String>) -> ApiError {
    ApiError::BadRequest(msg.into())
}
