use spring_web::axum::Json;
use spring_web::{get, post};
use spring_web::axum::response::IntoResponse;
use crate::http_service::error::app_err::AppError;
use crate::http_service::handlers;
use crate::http_service::handlers::hello_world;
use crate::http_service::model::http::UserResponse;

#[get("/hello")]
pub async fn hello() -> impl IntoResponse {
    Json(handlers::hello_world().await)
}


#[get("/screen")]
pub async fn sys_list() -> Result<impl IntoResponse,AppError> {
    Ok(handlers::screen().await.map_err(AppError::Internal))
}


// /// 创建用户
// #[post("/users")]
// pub async fn create_user(
//     State(state): State<AppState>,
//     Json(payload): Json<CreateUserRequest>,
// ) -> impl IntoResponse {
//     let user = state.create(payload).await;
//     Json(UserResponse::from(user))
// }
//
// /// 获取单个用户
// #[get("/users/:id")]
// pub async fn get_user(
//     State(state): State<AppState>,
//     Path(id): Path<u64>,
// ) -> impl IntoResponse {
//     match state.get(id).await {
//         Some(user) => Json(UserResponse::from(user)).into_response(),
//         None => (axum::http::StatusCode::NOT_FOUND, "User not found").into_response(),
//     }
// }