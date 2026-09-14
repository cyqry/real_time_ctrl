//! HTTP handler：执行 token 校验并把 Axum 请求/响应适配到 `RealCtrlApi`。
//!
//! 普通命令返回 JSON；截图便捷路由返回原始 PNG。日志不得输出 token、完整命令、路径或二进制内容。

use crate::api_contract::{ApiRequest, ApiResponse, MAX_API_BINARY_BYTES};
use crate::api_service::RealCtrlApi;
use crate::http_service::error::app_err::AppError;
use crate::input_command::{InputCommand, InputCtrlCommand, RemoteResp};
use anyhow::anyhow;
use bytes::Bytes;
use sha2::{Digest, Sha256};
use spring_web::axum::body::Body;
use spring_web::axum::http::{header, HeaderMap, StatusCode};
use spring_web::axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

pub async fn health_check() -> &'static str {
    "OK"
}

pub async fn execute_command(
    headers: &HeaderMap,
    api: &RealCtrlApi,
    request: ApiRequest,
) -> Result<ApiResponse, AppError> {
    authorize(headers)?;
    Ok(api.execute_request(request).await)
}

pub(crate) async fn screen(
    headers: &HeaderMap,
    api: &RealCtrlApi,
) -> Result<impl IntoResponse, AppError> {
    authorize(headers)?;

    let resp = api
        .execute(InputCommand::Ctrl(InputCtrlCommand::Screen("_".to_owned())))
        .await
        .map_err(AppError::from)?;

    let v = match resp {
        RemoteResp::SuccessData(v) => v,
        RemoteResp::Error(_, message) => return Err(AppError::BadRequest(message)),
        _ => return Err(AppError::Internal(anyhow!("截图返回了不支持的响应类型"))),
    };
    if v.len() > MAX_API_BINARY_BYTES {
        return Err(AppError::BadRequest("截图响应超过 HTTP 上限".to_string()));
    }

    let encoded_name =
        percent_encoding::utf8_percent_encode("screen.png", percent_encoding::NON_ALPHANUMERIC);
    let disposition = format!("attachment; filename=\"{}\"", encoded_name);
    let body = Bytes::from_owner(v);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CONTENT_DISPOSITION, disposition)
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-content-type-options", "nosniff")
        .body(Body::from(body))
        .map_err(|e| AppError::Internal(anyhow!(e)))
}

pub(crate) fn authorize(headers: &HeaderMap) -> Result<(), AppError> {
    let Some(expected) = configured_api_token() else {
        return Ok(());
    };

    let Some(actual) = token_from_headers(headers) else {
        return Err(AppError::Unauthorized("缺少本地 API token".to_string()));
    };

    if constant_time_eq(actual.as_bytes(), expected.as_bytes()) {
        Ok(())
    } else {
        Err(AppError::Unauthorized("本地 API token 无效".to_string()))
    }
}

fn configured_api_token() -> Option<String> {
    crate::runtime_config::api_token()
}

fn token_from_headers(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get("x-real-ctrl-token") {
        return value.to_str().ok().map(|v| v.to_string());
    }

    let authorization = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    authorization
        .strip_prefix("Bearer ")
        .map(|token| token.to_string())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    // 先哈希为固定长度，再使用经过审计的常量时间原语比较，避免手写循环被优化器改写。
    let left_digest = Sha256::digest(left);
    let right_digest = Sha256::digest(right);
    bool::from(left_digest.ct_eq(&right_digest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_checks_content_and_len() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
    }
}
