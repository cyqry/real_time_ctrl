use crate::api_contract::ApiErrorBody;
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
            AppError::Internal(err) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                ApiErrorBody::internal(err.to_string()),
            ),
        };

        let body = Json(json!({
            "ok": false,
            "error": error,
        }));

        (status, body).into_response()
    }
}
