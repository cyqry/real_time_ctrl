//! `real_ctrl` 与 `ctrl_server` 之间的活动帧。
//!
//! Ctrl 主连接使用命令、响应和心跳；CtrlData 使用数据帧。`DataAck` 由控制端发送，用于通知服务端
//! 释放下载路由，它不是文件内容校验成功的证明。

use crate::ctrl_resp::CmdResp;
use bytes::{Buf, BufMut, BytesMut};
use common::command::Command;
use common::protocol::{self, BufSerializable, ReqCmd};

pub const DATA_FRAME_CODE: u8 = 13;
const MAX_DATA_ID_BYTES: usize = protocol::MAX_CORRELATION_ID_BYTES;
pub const MAX_TARGET_ID_BYTES: usize = 128;

#[derive(Debug, Clone)]
/// 管理面 TLS 内允许的帧类型。
pub enum Frame {
    Cmd(ReqCmd),
    /// 指定目标的远端命令。服务端不能用会话当前选择替换此 ID，也不能降级成 Cmd。
    TargetedCmd(String, ReqCmd),
    /// 已认证主连接上的能力查询，字符串是用于关联响应的请求 ID。
    Capabilities(String),
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
            Frame::TargetedCmd(target, request) => {
                let mut bytes = BytesMut::new();
                bytes.put_u8(17);
                bytes.put_u32(target.len() as u32);
                bytes.put_slice(target.as_bytes());
                bytes.put(request.to_buf());
                bytes
            }
            Frame::Capabilities(id) => {
                let mut bytes = BytesMut::new();
                bytes.put_u8(18);
                bytes.put_u32(id.len() as u32);
                bytes.put_slice(id.as_bytes());
                bytes
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
            17 => {
                let target = take_identifier(&mut bys, MAX_TARGET_ID_BYTES)?;
                let request = ReqCmd::from_buf(bys)?;
                // Sys 在服务端执行，没有远端目标；拒绝混用，避免调用方误解安全语义。
                if matches!(request.get_cmd(), Command::Sys(_)) {
                    return None;
                }
                Some(Frame::TargetedCmd(target, request))
            }
            18 => {
                let id = take_identifier(&mut bys, protocol::MAX_CORRELATION_ID_BYTES)?;
                if !bys.is_empty() {
                    return None;
                }
                Some(Frame::Capabilities(id))
            }
            _ => None,
        }
    }
}

/// 长度在读取前校验；ID 不接受控制字符，也不会吞掉后续帧正文。
fn take_identifier(bytes: &mut BytesMut, maximum: usize) -> Option<String> {
    if bytes.len() < 4 {
        return None;
    }
    let length = bytes.get_u32() as usize;
    if length == 0 || length > maximum || bytes.len() < length {
        return None;
    }
    let id = String::from_utf8(bytes.split_to(length).to_vec()).ok()?;
    if id.trim().is_empty() || id.chars().any(char::is_control) {
        return None;
    }
    Some(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::{command::SysCommand, protocol::CmdOptions};

    #[test]
    fn targeted_command_round_trip_and_rejects_malformed_envelopes() {
        let request = ReqCmd::new(
            "request".into(),
            CmdOptions::default(),
            Command::Exec("echo test".into()),
        );
        let encoded = Frame::TargetedCmd("target-a".into(), request.clone()).to_buf();
        match Frame::from_buf(encoded.clone()).unwrap() {
            Frame::TargetedCmd(target, decoded) => {
                assert_eq!(target, "target-a");
                assert_eq!(decoded.get_id(), "request");
                assert!(matches!(decoded.get_cmd(), Command::Exec(text) if text == "echo test"));
            }
            _ => panic!("wrong frame"),
        }
        for end in 0..(5 + "target-a".len()) {
            assert!(Frame::from_buf(BytesMut::from(&encoded[..end])).is_none());
        }
        for target in [
            String::new(),
            " ".into(),
            "bad\nname".into(),
            "x".repeat(129),
        ] {
            assert!(
                Frame::from_buf(Frame::TargetedCmd(target, request.clone()).to_buf()).is_none()
            );
        }
        let system = ReqCmd::new(
            "request".into(),
            CmdOptions::default(),
            Command::Sys(SysCommand::Now),
        );
        assert!(Frame::from_buf(Frame::TargetedCmd("a".into(), system).to_buf()).is_none());
        let mut invalid_utf8 = encoded;
        invalid_utf8[5] = 0xff;
        assert!(Frame::from_buf(invalid_utf8).is_none());
    }

    #[test]
    fn capability_query_is_bounded_and_has_no_trailing_bytes() {
        let encoded = Frame::Capabilities("query".into()).to_buf();
        assert!(
            matches!(Frame::from_buf(encoded.clone()), Some(Frame::Capabilities(id)) if id == "query")
        );
        for end in 0..encoded.len() {
            assert!(Frame::from_buf(BytesMut::from(&encoded[..end])).is_none());
        }
        let mut trailing = encoded;
        trailing.put_u8(0);
        assert!(Frame::from_buf(trailing).is_none());
        for id in [String::new(), "x".repeat(129), "bad\0id".into()] {
            assert!(Frame::from_buf(Frame::Capabilities(id).to_buf()).is_none());
        }
    }

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
