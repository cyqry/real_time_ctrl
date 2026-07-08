use crate::api_contract::ApiResponse;
use crate::api_service::RealCtrlApi;
use crate::context::Context;
use crate::pipe::pipe_common::{
    deserialize_pipe_request, serialize_api_response, PipeRequest, ServerResponse,
    MAX_PIPE_REQUEST_BYTES, MAX_PIPE_RESPONSE_BYTES,
};
use anyhow::{anyhow, Context as AnyhowContext};
use common::protocol::transfer_b_encode;
use interprocess::os::windows::named_pipe::pipe_mode::Bytes;
use interprocess::os::windows::named_pipe::tokio::PipeStream;
use log::debug;
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub async fn handle_client(
    context: Context,
    mut stream: PipeStream<Bytes, Bytes>,
) -> anyhow::Result<()> {
    let api = RealCtrlApi::new(context);

    loop {
        let mut len_buf = [0u8; 4];
        if let Err(e) = stream.read_exact(&mut len_buf).await {
            return if e.kind() == io::ErrorKind::UnexpectedEof {
                Ok(())
            } else {
                Err(anyhow!(e))
            };
        }

        let msg_len = u32::from_be_bytes(len_buf) as usize;
        if msg_len > MAX_PIPE_REQUEST_BYTES {
            // 请求体过大时不继续读取 body，直接返回错误并关闭本次管道连接。
            write_legacy_response(
                &mut stream,
                &ServerResponse::Error(format!("请求过大: {} bytes", msg_len)),
            )
            .await?;
            return Ok(());
        }

        debug!("msg_len: {}", msg_len);
        let mut data = vec![0u8; msg_len];
        if let Err(e) = stream.read_exact(&mut data).await {
            return if e.kind() == io::ErrorKind::UnexpectedEof {
                Ok(())
            } else {
                Err(anyhow!(e))
            };
        }

        let request = deserialize_pipe_request(data.as_ref()).context("请求错误")?;
        match request {
            PipeRequest::Legacy(input_cmd) => {
                debug!("input_cmd: {:?}", input_cmd);
                let response = api
                    .execute(input_cmd)
                    .await
                    .map(ServerResponse::Success)
                    .unwrap_or_else(|e| ServerResponse::Error(format!("{}", e)));
                write_legacy_response(&mut stream, &response).await?;
            }
            PipeRequest::Api(api_request) => {
                debug!("api_request: {:?}", api_request);
                let response = api.execute_request(api_request).await;
                write_api_response(&mut stream, &response).await?;
            }
        }
    }
}

async fn write_legacy_response(
    stream: &mut PipeStream<Bytes, Bytes>,
    response: &ServerResponse,
) -> anyhow::Result<()> {
    let bys = postcard::to_allocvec(response)?;
    write_framed_response(stream, &bys).await
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
    if let Err(e) = stream.write_all(&bytes_mut).await {
        return if e.kind() == io::ErrorKind::UnexpectedEof {
            Ok(())
        } else {
            Err(anyhow!(e))
        };
    }
    Ok(())
}
