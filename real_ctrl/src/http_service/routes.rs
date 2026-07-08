use crate::api_contract::{ApiRequest, ApiResponse};
use crate::api_service::RealCtrlApi;
use crate::http_service::error::app_err::AppError;
use crate::http_service::handlers;
use spring_web::axum::http::HeaderMap;
use spring_web::axum::response::IntoResponse;
use spring_web::axum::Json;
use spring_web::extractor::Component;
use spring_web::{get, post};

#[get("/health")]
pub async fn health() -> impl IntoResponse {
    handlers::health_check().await
}

#[get("/hello")]
pub async fn hello() -> impl IntoResponse {
    Json(handlers::hello_world().await)
}

#[post("/v1/commands")]
pub async fn execute_command(
    headers: HeaderMap,
    Component(api): Component<RealCtrlApi>,
    Json(request): Json<ApiRequest>,
) -> Result<Json<ApiResponse>, AppError> {
    Ok(Json(
        handlers::execute_command(&headers, &api, request).await?,
    ))
}

#[get("/screen")]
pub async fn screen(
    headers: HeaderMap,
    Component(api): Component<RealCtrlApi>,
) -> Result<impl IntoResponse, AppError> {
    handlers::screen(&headers, &api).await
}
