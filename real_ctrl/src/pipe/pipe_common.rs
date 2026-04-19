use bytes::BytesMut;
use serde::{Deserialize, Serialize};
use common::protocol::BufSerializable;
use crate::input_command::RemoteResp;

// 定义管道名称（Windows 格式）
pub const PIPE_NAME: &str = r"\\.\pipe\real_ctrl_service_pipe";

#[derive(Serialize, Deserialize, Debug)]
pub enum  ServerResponse{
    Success(RemoteResp),
    Error(String)
}

impl BufSerializable for ServerResponse {
    fn to_buf(&self) -> BytesMut {
        todo!()
    }

    fn from_buf(bys: BytesMut) -> Option<Self>
    where
        Self: Sized
    {
        todo!()
    }
}