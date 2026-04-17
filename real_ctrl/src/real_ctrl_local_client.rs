
use std::io::{Read, Write};
use anyhow::Result;
use interprocess::local_socket::prelude::LocalSocketStream;
use interprocess::local_socket::ToFsName;
use interprocess::local_socket::traits::Stream;
use interprocess::os::windows::local_socket::NamedPipe;
use crate::input_command::{serialize_command, InputCommand, RemoteResp};

mod pipe;
mod context;

mod input_command;

const PIPE_NAME: &str = r"\\.\pipe\my_service_pipe";

/// 调用远程方法
fn call_method(method: &str, args: &InputCommand) -> Result<RemoteResp> {
    // 1. 连接到服务端管道
    let mut stream = LocalSocketStream::connect(PIPE_NAME.to_fs_name::<NamedPipe>().unwrap())?;

    // 2. 构造请求
    let req_data = serialize_command(args)?;

    let req_len = (req_data.len() as u32).to_le_bytes();

    // 3. 发送请求
    stream.write_all(&req_len)?;
    stream.write_all(&req_data)?;
    stream.flush()?;

    // 4. 读取响应长度
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let resp_len = u32::from_le_bytes(len_buf) as usize;

    // 5. 读取响应数据
    let mut resp_data = vec![0u8; resp_len];
    stream.read_exact(&mut resp_data)?;

    // 6. 反序列化响应
    let resp: RemoteResp = serde_json::from_slice(&resp_data)?;
    Ok(resp)
}

fn main() -> Result<()> {
   
    Ok(())
}