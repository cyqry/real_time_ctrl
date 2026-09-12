use crate::api_contract::{ApiErrorBody, ApiResponse};
use crate::api_service::RealCtrlApi;
use crate::context::Context;
use crate::pipe::pipe_common::{
    deserialize_pipe_request, serialize_api_response, MAX_PIPE_REQUEST_BYTES,
    MAX_PIPE_RESPONSE_BYTES, PIPE_IO_TIMEOUT,
};
use anyhow::anyhow;
use common::protocol::transfer_b_encode;
use interprocess::os::windows::named_pipe::pipe_mode::Bytes;
use interprocess::os::windows::named_pipe::tokio::PipeStream;
use log::debug;
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

pub async fn handle_client(
    context: Context,
    mut stream: PipeStream<Bytes, Bytes>,
) -> anyhow::Result<()> {
    let api = RealCtrlApi::new(context);

    loop {
        let mut len_buf = [0u8; 4];
        let read_len = timeout(PIPE_IO_TIMEOUT, stream.read_exact(&mut len_buf))
            .await
            .map_err(|_| anyhow!("读取管道请求长度超时"))?;
        if let Err(e) = read_len {
            return if e.kind() == io::ErrorKind::UnexpectedEof {
                Ok(())
            } else {
                Err(anyhow!(e))
            };
        }

        let msg_len = u32::from_be_bytes(len_buf) as usize;
        if msg_len > MAX_PIPE_REQUEST_BYTES {
            // 请求体过大时不继续读取 body，直接返回错误并关闭本次管道连接。
            write_api_response(
                &mut stream,
                &ApiResponse::error(
                    None,
                    ApiErrorBody::payload_too_large(format!("请求过大: {} bytes", msg_len)),
                ),
            )
            .await?;
            return Ok(());
        }

        debug!("msg_len: {}", msg_len);
        let mut data = vec![0u8; msg_len];
        let read_body = timeout(PIPE_IO_TIMEOUT, stream.read_exact(&mut data))
            .await
            .map_err(|_| anyhow!("读取管道请求体超时"))?;
        if let Err(e) = read_body {
            return if e.kind() == io::ErrorKind::UnexpectedEof {
                Ok(())
            } else {
                Err(anyhow!(e))
            };
        }

        match deserialize_pipe_request(data.as_ref()) {
            Ok(api_request) => {
                debug!("api_request: {:?}", api_request);
                let response = api.execute_request(api_request).await;
                write_api_response(&mut stream, &response).await?;
            }
            Err(error) => {
                let response = ApiResponse::error(
                    None,
                    ApiErrorBody::bad_request(format!("管道请求错误: {error}")),
                );
                write_api_response(&mut stream, &response).await?;
            }
        }
    }
}

async fn write_api_response(
    stream: &mut PipeStream<Bytes, Bytes>,
    response: &ApiResponse,
) -> anyhow::Result<()> {
    let bys = serialize_api_response(response)?;
    write_framed_response(stream, &bys).await
}

async fn write_framed_response(
    stream: &mut PipeStream<Bytes, Bytes>,
    bys: &[u8],
) -> anyhow::Result<()> {
    if bys.len() > MAX_PIPE_RESPONSE_BYTES {
        return Err(anyhow!("响应过大: {} bytes", bys.len()));
    }

    let bytes_mut = transfer_b_encode(bys, 0, bys.len());
    let write_result = timeout(PIPE_IO_TIMEOUT, async {
        stream.write_all(&bytes_mut).await?;
        stream.flush().await
    })
    .await
    .map_err(|_| anyhow!("写管道响应超时"))?;
    if let Err(e) = write_result {
        return if e.kind() == io::ErrorKind::UnexpectedEof {
            Ok(())
        } else {
            Err(anyhow!(e))
        };
    }
    Ok(())
}
