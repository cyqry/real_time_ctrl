use crate::input_command::{serialize_command, InputCommand, RemoteResp};
use interprocess::local_socket::{tokio::{prelude::*}, GenericNamespaced, ListenerOptions};
use interprocess::local_socket::tokio::Stream;
use interprocess::os::windows::named_pipe::PipeDirection::Duplex;
use interprocess::os::windows::named_pipe::tokio::DuplexPipeStream;
use log::debug;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use crate::pipe::pipe_common::{ServerResponse, PIPE_NAME};

/// 调用远程方法
pub async fn invoke(args: &InputCommand) -> anyhow::Result<ServerResponse> {
    let conn = DuplexPipeStream::connect_by_path(PIPE_NAME).await?;
    let (reader, mut sender) = conn.split();
    let mut receiver = BufReader::new(reader);
    let req_data = serialize_command(args)?;
    let req_len = (req_data.len() as u32).to_be_bytes();
    sender.write_all(&req_len).await?;
    sender.write_all(&req_data).await?;
    sender.flush().await?;
    debug!("发送请求完成");
    let mut len_buf = [0u8; 4];
    receiver.read_exact(&mut len_buf).await?;
    let resp_len = u32::from_be_bytes(len_buf) as usize;
    let mut resp_data = vec![0u8; resp_len];
    receiver.read_exact(&mut resp_data).await?;
    let resp: ServerResponse = postcard::from_bytes(&resp_data)?;
    Ok(resp)
}