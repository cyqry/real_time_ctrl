//! `real_ctrl` 与 `ctrl_server` 之间的活动帧。
//!
//! Ctrl 主连接使用命令、响应和心跳；CtrlData 使用数据帧。`DataAck` 由控制端发送，用于通知服务端
//! 释放下载路由，它不是文件内容校验成功的证明。

use crate::ctrl_resp::CmdResp;
use bytes::{Buf, BufMut, BytesMut};
use common::protocol::{self, BufSerializable, ReqCmd};

pub const DATA_FRAME_CODE: u8 = 13;
const MAX_DATA_ID_BYTES: usize = protocol::MAX_CORRELATION_ID_BYTES;

#[derive(Debug, Clone)]
/// 管理面 TLS 内允许的帧类型。
pub enum Frame {
    Cmd(ReqCmd),
    Resp(CmdResp),

    /// 数据 ID 与原始 payload，只允许在已绑定会话的 CtrlData 连接上传输。
    Data(String, BytesMut),
    /// 控制端确认下载数据已经消费完成，服务端据此尽早释放长传输路由。
    DataAck(String),

    Ping,
    Pong,
}

/// 控制端数据通道热路径编码，避免大 payload 在两层封帧间重复复制。
pub fn encode_data_frame(data_id: &str, data: &[u8]) -> std::io::Result<BytesMut> {
    protocol::transfer_encode_data_frame(DATA_FRAME_CODE, data_id, data, MAX_DATA_ID_BYTES)
}

impl BufSerializable for Frame {
    fn to_buf(&self) -> BytesMut {
        match self {
            Frame::Cmd(req_cmd) => {
                let mut bytes_mut = BytesMut::new();
                bytes_mut.put_u8(11);
                bytes_mut.put(req_cmd.to_buf());
                bytes_mut
            }
            Frame::Resp(cmd_resp) => {
                let mut bytes_mut = BytesMut::new();
                bytes_mut.put_u8(12);
                bytes_mut.put(cmd_resp.to_buf());
                bytes_mut
            }
            Frame::Data(data_id, bys) => {
                let mut bytes_mut = BytesMut::with_capacity(bys.len() + 1);
                bytes_mut.put_u8(13);

                let id_bys = data_id.as_bytes();
                bytes_mut.put_u32(id_bys.len() as u32);
                bytes_mut.put_slice(id_bys);
                bytes_mut.put_slice(bys);
                bytes_mut
            }
            Frame::Ping => {
                let mut bytes_mut = BytesMut::new();
                bytes_mut.put_u8(14);
                bytes_mut
            }
            Frame::Pong => {
                let mut bytes_mut = BytesMut::new();
                bytes_mut.put_u8(15);
                bytes_mut
            }
            Frame::DataAck(data_id) => {
                let mut bytes_mut = BytesMut::with_capacity(5 + data_id.len());
                bytes_mut.put_u8(16);
                bytes_mut.put_u32(data_id.len() as u32);
                bytes_mut.put_slice(data_id.as_bytes());
                bytes_mut
            }
        }
    }

    fn from_buf(mut bys: BytesMut) -> Option<Self> {
        if bys.remaining() < 1 {
            return None;
        }
        let code = bys.get_u8();
        match code {
            11 => Some(Frame::Cmd(ReqCmd::from_buf(bys)?)),
            12 => Some(Frame::Resp(CmdResp::from_buf(bys)?)),
            13 => {
                if bys.remaining() < 4 {
                    return None;
                }
                let id_len = bys.get_u32() as usize;
                if id_len == 0 || id_len > MAX_DATA_ID_BYTES || bys.len() < id_len {
                    return None;
                }
                let id_bys = bys.split_to(id_len);
                let data_id = String::from_utf8(id_bys.to_vec()).ok()?;
                Some(Frame::Data(data_id, bys))
            }
            14 if bys.is_empty() => Some(Frame::Ping),
            15 if bys.is_empty() => Some(Frame::Pong),
            16 => {
                if bys.remaining() < 4 {
                    return None;
                }
                let id_len = bys.get_u32() as usize;
                if id_len == 0 || id_len > MAX_DATA_ID_BYTES || bys.remaining() != id_len {
                    return None;
                }
                Some(Frame::DataAck(String::from_utf8(bys.to_vec()).ok()?))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_frame_returns_none() {
        assert!(Frame::from_buf(BytesMut::new()).is_none());
    }

    #[test]
    fn short_data_frame_returns_none() {
        let mut bys = BytesMut::new();
        bys.put_u8(13);
        bys.put_u8(1);

        assert!(Frame::from_buf(bys).is_none());
    }

    #[test]
    fn data_frame_round_trip() {
        let frame = Frame::Data("data-id".to_string(), BytesMut::from(&b"hello"[..]));
        let decoded = Frame::from_buf(frame.to_buf()).unwrap();

        match decoded {
            Frame::Data(id, data) => {
                assert_eq!(id, "data-id");
                assert_eq!(data.as_ref(), b"hello");
            }
            _ => panic!("unexpected frame"),
        }
    }

    #[test]
    fn direct_data_encoder_includes_length_prefix() {
        let mut encoded = encode_data_frame("data-id", b"hello").unwrap();
        let frame_len = encoded.get_u32() as usize;
        assert_eq!(frame_len, encoded.len());
        match Frame::from_buf(encoded) {
            Some(Frame::Data(id, data)) => {
                assert_eq!(id, "data-id");
                assert_eq!(data.as_ref(), b"hello");
            }
            _ => panic!("unexpected frame"),
        }
    }

    #[test]
    fn data_ack_round_trip_and_rejects_trailing_bytes() {
        let frame = Frame::DataAck("data-id".to_string());
        assert!(
            matches!(Frame::from_buf(frame.to_buf()), Some(Frame::DataAck(id)) if id == "data-id")
        );
        let mut invalid = frame.to_buf();
        invalid.put_u8(1);
        assert!(Frame::from_buf(invalid).is_none());
    }
}
