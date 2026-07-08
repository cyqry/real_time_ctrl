use crate::api_contract::{ApiRequest, ApiResponse};
use crate::input_command::{serialize_command, InputCommand};
use crate::pipe::pipe_common::{
    deserialize_api_response, serialize_api_request, ServerResponse, MAX_PIPE_REQUEST_BYTES,
    MAX_PIPE_RESPONSE_BYTES, PIPE_NAME,
};
use interprocess::os::windows::named_pipe::tokio::DuplexPipeStream;
use log::debug;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};

/// 调用旧版本地管道协议，保留给已有调用方兼容使用。
pub async fn invoke(args: &InputCommand) -> anyhow::Result<ServerResponse> {
    let req_data = serialize_command(args)?;
    let resp_data = invoke_raw(req_data).await?;
    let resp: ServerResponse = postcard::from_bytes(&resp_data)?;
    Ok(resp)
}

/// 调用新版本地管道开放 API，契约与 HTTP 的 ApiRequest/ApiResponse 保持一致。
pub async fn invoke_api(request: &ApiRequest) -> anyhow::Result<ApiResponse> {
    let req_data = serialize_api_request(request)?;
    let resp_data = invoke_raw(req_data).await?;
    deserialize_api_response(&resp_data)
}

async fn invoke_raw(req_data: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    let conn = DuplexPipeStream::connect_by_path(PIPE_NAME).await?;
    let (reader, mut sender) = conn.split();
    let mut receiver = BufReader::new(reader);

    if req_data.len() > MAX_PIPE_REQUEST_BYTES {
        return Err(anyhow::anyhow!("请求过大: {} bytes", req_data.len()));
    }
    let req_len = (req_data.len() as u32).to_be_bytes();
    sender.write_all(&req_len).await?;
    sender.write_all(&req_data).await?;
    sender.flush().await?;
    debug!("发送请求完成");

    let mut len_buf = [0u8; 4];
    receiver.read_exact(&mut len_buf).await?;
    let resp_len = u32::from_be_bytes(len_buf) as usize;
    if resp_len > MAX_PIPE_RESPONSE_BYTES {
        return Err(anyhow::anyhow!("响应过大: {} bytes", resp_len));
    }
    let mut resp_data = vec![0u8; resp_len];
    receiver.read_exact(&mut resp_data).await?;
    Ok(resp_data)
}
