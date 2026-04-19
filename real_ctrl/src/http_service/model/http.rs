use serde::{Deserialize, Serialize};
use spring_web::axum::response::{IntoResponse, Response};

#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    pub name: String,
    pub email: String,
}

#[derive(Debug, Serialize)]
pub struct UserResponse {
    pub id: u64,
    pub name: String,
    pub email: String,
}
