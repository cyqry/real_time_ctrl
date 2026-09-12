use crate::api_contract::{ApiRequest, ApiResponse};
use std::time::Duration;

// 定义管道名称（Windows 命名管道格式）。
pub const PIPE_NAME: &str = r"\\.\pipe\real_ctrl_service_pipe";
/// 本地管道请求只承载命令模型，不应携带大块文件内容。
pub const MAX_PIPE_REQUEST_BYTES: usize = 1024 * 1024;
/// 响应需要兼容截图等二进制数据，先保留 64 MiB 上限。
pub const MAX_PIPE_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
pub const PIPE_IO_TIMEOUT: Duration = Duration::from_secs(30);

const PIPE_API_MAGIC: &[u8] = b"RTCAPI1\0";

pub fn serialize_api_request(request: &ApiRequest) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(PIPE_API_MAGIC.len() + 256);
    bytes.extend_from_slice(PIPE_API_MAGIC);
    bytes.extend_from_slice(&serde_json::to_vec(request)?);
    Ok(bytes)
}

pub fn serialize_api_response(response: &ApiResponse) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(PIPE_API_MAGIC.len() + 256);
    bytes.extend_from_slice(PIPE_API_MAGIC);
    bytes.extend_from_slice(&serde_json::to_vec(response)?);
    Ok(bytes)
}

pub fn deserialize_pipe_request(bytes: &[u8]) -> anyhow::Result<ApiRequest> {
    let rest = bytes
        .strip_prefix(PIPE_API_MAGIC)
        .ok_or_else(|| anyhow::anyhow!("管道请求缺少 RTCAPI1 magic 前缀"))?;
    Ok(serde_json::from_slice::<ApiRequest>(rest)?)
}

pub fn deserialize_api_response(bytes: &[u8]) -> anyhow::Result<ApiResponse> {
    let rest = bytes
        .strip_prefix(PIPE_API_MAGIC)
        .ok_or_else(|| anyhow::anyhow!("新版管道响应缺少 magic 前缀"))?;
    Ok(serde_json::from_slice::<ApiResponse>(rest)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api_contract::{ApiCommand, ApiRequest};

    #[test]
    fn api_pipe_request_requires_magic() {
        let req = ApiRequest::new(ApiCommand::SysNow);
        let bytes = serialize_api_request(&req).unwrap();
        let decoded = deserialize_pipe_request(&bytes).unwrap();
        assert_eq!(decoded.version, 1);
    }

    #[test]
    fn pipe_request_without_magic_is_rejected() {
        assert!(deserialize_pipe_request(br#"{"version":1}"#).is_err());
    }
}
