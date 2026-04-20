use std::io;
use crate::input_command::{deserialize_command, InputCommand, RemoteResp};
use anyhow::{anyhow, Context as AnyhowContext};
use std::io::Read;
use std::io::Write;
use bytes::{BufMut, BytesMut};
use interprocess::os::windows::named_pipe::pipe_mode::Bytes;
use interprocess::os::windows::named_pipe::tokio::PipeStream;
use log::debug;
use serde::{Deserialize, Serialize};
use serde::de::Unexpected::Option;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use common::protocol::{transfer_b_encode, transfer_encode};
use crate::context::Context;
use crate::dispatch;
use crate::pipe::pipe_common::ServerResponse;

// 处理单个连接
//todo 控制不能并发调用
pub async fn handle_client(context: Context, mut stream: PipeStream<Bytes, Bytes>) -> anyhow::Result<()> {
    loop {
        let mut len_buf = [0u8; 4];
        //async trait 封装了OK(0)为 eof error
        if let Err(e) = stream.read_exact(&mut len_buf).await { return if e.kind() == io::ErrorKind::UnexpectedEof { Ok(()) } else { Err(anyhow!(e)) }; }

        let msg_len = u32::from_be_bytes(len_buf) as usize;
        debug!("msg_len: {}", msg_len);
        let mut data = vec![0u8; msg_len];
        if let Err(e) = stream.read_exact(&mut data).await { return if e.kind() == io::ErrorKind::UnexpectedEof { Ok(()) } else { Err(anyhow!(e)) }; }
        debug!("data: {:?}", data);

        let input_cmd: InputCommand = deserialize_command(data.as_ref()).context("请求错误")?;

        debug!("input_cmd: {:?}", input_cmd);
        let res = dispatch::distribution_other(&context, input_cmd).await;
        let response = res.and_then(|resp| Ok(ServerResponse::Success(resp))).unwrap_or_else(|e| ServerResponse::Error(format!("{}", e)));
        let bys = postcard::to_allocvec(&response)?;
        let bytes_mut = transfer_b_encode(&bys, 0, bys.len());
        if let Err(e) = stream.write_all(&bytes_mut).await { return if e.kind() == io::ErrorKind::UnexpectedEof { Ok(()) } else { Err(anyhow!(e)) }; };
    } 
  
    //
    //
    // // 5. 构造响应并发送
    // let resp = Response {
    //     result: result_vec,
    //     error,
    // };
    // let resp_data = serde_json::to_vec(&resp)?;
    // let resp_len = (resp_data.len() as u32).to_le_bytes();
    // stream.write_all(&resp_len)?;
    // stream.write_all(&resp_data)?;
    // stream.flush()?;

    Ok(())
}