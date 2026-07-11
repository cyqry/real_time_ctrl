use crate::api_contract::{ApiRequest, ApiResponse};
use crate::input_command::{deserialize_command, InputCommand, RemoteResp};
use bytes::BytesMut;
use common::protocol::BufSerializable;
use serde::{Deserialize, Serialize};
use std::time::Duration;

// 定义管道名称（Windows 命名管道格式）。
pub const PIPE_NAME: &str = r"\\.\pipe\real_ctrl_service_pipe";
/// 本地管道请求只承载命令模型，不应携带大块文件内容。
pub const MAX_PIPE_REQUEST_BYTES: usize = 1024 * 1024;
/// 响应需要兼容截图等二进制数据，先保留 64 MiB 上限。
pub const MAX_PIPE_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
pub const PIPE_IO_TIMEOUT: Duration = Duration::from_secs(30);

const PIPE_API_MAGIC: &[u8] = b"RTCAPI1\0";

#[derive(Serialize, Deserialize, Debug)]
pub enum ServerResponse {
    Success(RemoteResp),
    Error(String),
}

#[derive(Debug)]
pub enum PipeRequest {
    Legacy(InputCommand),
    Api(ApiRequest),
}

impl BufSerializable for ServerResponse {
    fn to_buf(&self) -> BytesMut {
        let vec = postcard::to_allocvec(self).expect("failed to serialize ServerResponse");
        BytesMut::from(vec.as_slice())
    }

    fn from_buf(bys: BytesMut) -> Option<Self>
    where
        Self: Sized,
    {
        postcard::from_bytes::<ServerResponse>(&bys).ok()
    }
}

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

pub fn deserialize_pipe_request(bytes: &[u8]) -> anyhow::Result<PipeRequest> {
    if let Some(rest) = bytes.strip_prefix(PIPE_API_MAGIC) {
        // 新版开放 API 使用 JSON，避免 postcard 对 serde 内部标签枚举的限制。
        let request = serde_json::from_slice::<ApiRequest>(rest)?;
        return Ok(PipeRequest::Api(request));
    }

    // 无 magic 前缀时按旧版 InputCommand 解析，保证已有本地客户端不被破坏。
    let command = deserialize_command(bytes)?;
    Ok(PipeRequest::Legacy(command))
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
    use crate::input_command::{serialize_command, InputCtrlCommand};

    #[test]
    fn legacy_pipe_request_still_decodes() {
        let command = InputCommand::Ctrl(InputCtrlCommand::Ls("C:\\".to_string()));
        let bytes = serialize_command(&command).unwrap();
        match deserialize_pipe_request(&bytes).unwrap() {
            PipeRequest::Legacy(InputCommand::Ctrl(InputCtrlCommand::Ls(path))) => {
                assert_eq!(path, "C:\\")
            }
            _ => panic!("旧版管道请求解析错误"),
        }
    }

    #[test]
    fn api_pipe_request_requires_magic() {
        let req = ApiRequest::new(ApiCommand::SysNow);
        let bytes = serialize_api_request(&req).unwrap();
        match deserialize_pipe_request(&bytes).unwrap() {
            PipeRequest::Api(decoded) => assert_eq!(decoded.version, 1),
            _ => panic!("新版管道请求解析错误"),
        }
    }
}
