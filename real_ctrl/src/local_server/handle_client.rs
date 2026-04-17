use crate::context::Context;
use crate::dispatch;
use crate::input_command::{deserialize_command, InputCommand};
use anyhow::Context as AnyhowContext;
use interprocess::local_socket::prelude::LocalSocketStream;
use std::io::Read;


// 处理单个连接
pub async fn handle_client(context: &Context, mut stream: LocalSocketStream) -> anyhow::Result<()> {
    // 1. 读取消息长度（4字节，小端序）
    loop {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf)?;
        let msg_len = u32::from_le_bytes(len_buf) as usize;

        let mut data = vec![0u8; msg_len];
        stream.read_exact(&mut data)?;

        let input_cmd: InputCommand = deserialize_command(data.as_ref()).context("请求错误")?;

        // let mut bytes_mut = BytesMut::with_capacity(data.len());
        // bytes_mut.put_slice(&data);
        // // 3. 反序列化
        // let input_cmd = RemoteResp::from_buf(bytes_mut).ok_or(anyhow!("响应错误"))?;
        postcard::to_allocvec(&Some(1))?;
        postcard::to_allocvec(&Some(1))?;
        let res = dispatch::distribution_other(context, input_cmd).await;
        // let bytes_mut = transfer_encode();
        // stream.write_all(&bytes_mut)?;
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