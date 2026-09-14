//! Windows 命名管道 API 客户端。
//!
//! 每次调用建立独立双工管道连接，发送版本化 JSON 请求并读取一条有长度边界的响应；这使多个调用任务
//! 可以自然并行，而无需共享一个响应 Receiver。

use crate::api_contract::{ApiRequest, ApiResponse};
use crate::pipe::pipe_common::{
    deserialize_api_response, serialize_api_request, MAX_PIPE_REQUEST_BYTES,
    MAX_PIPE_RESPONSE_BYTES, PIPE_IO_TIMEOUT, PIPE_NAME,
};
use interprocess::os::windows::named_pipe::tokio::DuplexPipeStream;
use log::debug;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

/// 调用本地管道开放 API，契约与 HTTP 的 ApiRequest/ApiResponse 保持一致。
pub async fn invoke_api(request: &ApiRequest) -> anyhow::Result<ApiResponse> {
    let req_data = serialize_api_request(request)?;
    let resp_data = invoke_raw(req_data).await?;
    deserialize_api_response(&resp_data)
}

async fn invoke_raw(req_data: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    let conn = timeout(
        PIPE_IO_TIMEOUT,
        DuplexPipeStream::connect_by_path(PIPE_NAME),
    )
    .await
    .map_err(|_| anyhow::anyhow!("连接本地管道超时"))??;
    let (reader, mut sender) = conn.split();
    let mut receiver = BufReader::new(reader);

    if req_data.len() > MAX_PIPE_REQUEST_BYTES {
        return Err(anyhow::anyhow!("请求过大: {} bytes", req_data.len()));
    }
    let req_len = (req_data.len() as u32).to_be_bytes();
    timeout(PIPE_IO_TIMEOUT, async {
        sender.write_all(&req_len).await?;
        sender.write_all(&req_data).await?;
        sender.flush().await
    })
    .await
    .map_err(|_| anyhow::anyhow!("写本地管道请求超时"))??;
    debug!("发送请求完成");

    let mut len_buf = [0u8; 4];
    timeout(PIPE_IO_TIMEOUT, receiver.read_exact(&mut len_buf))
        .await
        .map_err(|_| anyhow::anyhow!("读取本地管道响应长度超时"))??;
    let resp_len = u32::from_be_bytes(len_buf) as usize;
    if resp_len > MAX_PIPE_RESPONSE_BYTES {
        return Err(anyhow::anyhow!("响应过大: {} bytes", resp_len));
    }
    let mut resp_data = vec![0u8; resp_len];
    timeout(PIPE_IO_TIMEOUT, receiver.read_exact(&mut resp_data))
        .await
        .map_err(|_| anyhow::anyhow!("读取本地管道响应体超时"))??;
    Ok(resp_data)
}
