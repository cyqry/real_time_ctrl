use crate::api_contract::ApiErrorBody;
use crate::api_service::ApiServiceError;
use serde_json::json;
use spring_web::axum::http::StatusCode;
use spring_web::axum::response::{IntoResponse, Response};
use spring_web::axum::Json;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Resource not found: {0}")]
    NotFound(String),

    #[error("Invalid input: {0}")]
    BadRequest(String),

    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    #[error("Forbidden: {0}")]
    Forbidden(String),

    #[error("Conflict: {0}")]
    Conflict(String),

    #[error("Internal server error")]
    Internal(anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error) = match self {
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, ApiErrorBody::not_found(msg)),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, ApiErrorBody::bad_request(msg)),
            AppError::Unauthorized(msg) => {
                (StatusCode::UNAUTHORIZED, ApiErrorBody::unauthorized(msg))
            }
            AppError::Forbidden(msg) => (StatusCode::FORBIDDEN, ApiErrorBody::forbidden(msg)),
            AppError::Conflict(msg) => (StatusCode::CONFLICT, ApiErrorBody::busy(msg)),
            AppError::Internal(err) => {
                log::error!("HTTP API 内部错误: {}", err);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ApiErrorBody::internal("内部服务错误"),
                )
            }
        };

        let body = Json(json!({
            "ok": false,
            "error": error,
        }));

        (status, body).into_response()
    }
}

impl From<ApiServiceError> for AppError {
    fn from(error: ApiServiceError) -> Self {
        match error {
            ApiServiceError::Busy => Self::Conflict("另一个控制命令正在执行".to_string()),
            ApiServiceError::Forbidden(message) => Self::Forbidden(message),
            ApiServiceError::Execution(error) => Self::Internal(error),
        }
    }
}
