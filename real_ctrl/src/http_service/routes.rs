use crate::api_contract::{ApiRequest, ApiResponse};
use crate::api_service::RealCtrlApi;
use crate::http_service::error::app_err::AppError;
use crate::http_service::handlers;
use spring_web::axum::http::HeaderMap;
use spring_web::axum::response::IntoResponse;
use spring_web::axum::routing::{get, post};
use spring_web::axum::Json;
use spring_web::extractor::Component;
use spring_web::Router;

pub async fn health() -> impl IntoResponse {
    handlers::health_check().await
}

pub async fn execute_command(
    headers: HeaderMap,
    Component(api): Component<RealCtrlApi>,
    Json(request): Json<ApiRequest>,
) -> Result<Json<ApiResponse>, AppError> {
    Ok(Json(
        handlers::execute_command(&headers, &api, request).await?,
    ))
}

pub async fn screen(
    headers: HeaderMap,
    Component(api): Component<RealCtrlApi>,
) -> Result<impl IntoResponse, AppError> {
    handlers::screen(&headers, &api).await
}

/// 显式构造路由，避免依赖跨 crate 的 inventory 链接副作用。
///
/// `real_ctrl` 作为 library 被多个 bin 复用后，只有 inventory 注册而没有符号引用的
/// 对象可能不被最终链接器拉入。公开 API 因此必须由启动组合根显式安装。
pub fn router() -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/commands", post(execute_command))
        .route("/screen", get(screen))
}
