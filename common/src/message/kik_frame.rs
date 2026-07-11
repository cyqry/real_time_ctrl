use crate::command::Command;
use crate::message::kik_frame::KikFrame::*;
use crate::message::kik_resp::KikResp;
use crate::protocol::{BufSerializable, ReqCmd};
use bytes::{Buf, BufMut, BytesMut};

const MAX_DATA_ID_BYTES: usize = 128;

#[derive(Debug, Clone)]
pub enum KikFrame {
    //这个目前不使用了，使用ReqCmd替代
    CmdExtra(Command, String),
    RespExtra(KikResp, String),
    Cmd(ReqCmd),

    Data(String, BytesMut), //数据传输的data帧

    Ping,
    Pong,
}

impl BufSerializable for KikFrame {
    fn to_buf(&self) -> BytesMut {
        match self {
            CmdExtra(cmd, s) => {
                let mut bytes_mut = BytesMut::new();
                bytes_mut.put_u8(14);
                let cmd_buf = cmd.to_buf();
                let cmd_len = cmd_buf.len() as u32;
                bytes_mut.put_u32(cmd_len);
                bytes_mut.put(cmd_buf);
                bytes_mut.put_slice(s.as_bytes());
                bytes_mut
            }
            RespExtra(resp, s) => {
                let mut bytes_mut = BytesMut::new();
                bytes_mut.put_u8(13);
                let resp_buf = resp.to_buf();
                let resp_len = resp_buf.len() as u32;
                bytes_mut.put_u32(resp_len);
                bytes_mut.put(resp_buf);
                bytes_mut.put_slice(s.as_bytes());
                bytes_mut
            }
            Data(data_id, bys) => {
                let mut bytes_mut = BytesMut::with_capacity(bys.len() + 1);
                bytes_mut.put_u8(12);

                let id_bys = data_id.as_bytes();
                bytes_mut.put_u32(id_bys.len() as u32);
                bytes_mut.put_slice(id_bys);
                bytes_mut.put_slice(bys);
                bytes_mut
            }
            Cmd(command) => {
                let mut bytes_mut = BytesMut::new();
                bytes_mut.put_u8(11);
                bytes_mut.put(command.to_buf());
                bytes_mut
            }
            Ping => {
                let mut bytes_mut = BytesMut::with_capacity(1);
                bytes_mut.put_u8(1);
                bytes_mut
            }
            Pong => {
                let mut bytes_mut = BytesMut::with_capacity(1);
                bytes_mut.put_u8(0);
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
            14 => {
                if bys.remaining() < 4 {
                    return None;
                }
                let len = bys.get_u32();
                if len == 0 || len as usize > 64 * 1024 || bys.remaining() < len as usize {
                    return None;
                }
                let cmd = Command::from_buf(bys.split_to(len as usize))?;
                if bys.is_empty() || bys.len() > MAX_DATA_ID_BYTES {
                    return None;
                }
                Some(CmdExtra(cmd, String::from_utf8(bys.to_vec()).ok()?))
            }
            13 => {
                if bys.remaining() < 4 {
                    return None;
                }
                let len = bys.get_u32();
                if len == 0
                    || len as usize > crate::ltc_codec::CONTROL_MAX_FRAME_LENGTH
                    || bys.remaining() < len as usize
                {
                    return None;
                }
                let resp = KikResp::from_buf(bys.split_to(len as usize))?;
                if bys.is_empty() || bys.len() > MAX_DATA_ID_BYTES {
                    return None;
                }
                Some(RespExtra(resp, String::from_utf8(bys.to_vec()).ok()?))
            }
            12 => {
                if bys.remaining() < 4 {
                    return None;
                }
                let id_len = bys.get_u32();
                if id_len == 0
                    || id_len as usize > MAX_DATA_ID_BYTES
                    || bys.remaining() < id_len as usize
                {
                    return None;
                }
                let id = String::from_utf8(bys.split_to(id_len as usize).to_vec()).ok()?;
                Some(Data(id, bys))
            }
            11 => Some(Cmd(ReqCmd::from_buf(bys)?)),
            1 if bys.is_empty() => Some(Ping),
            0 if bys.is_empty() => Some(Pong),
            _ => None,
        }
    }
}

#[test]
fn test() {
    use crate::message::kik_resp::ClientSuccessResp;
    let bytes_mut = KikFrame::RespExtra(
        KikResp::Success(ClientSuccessResp::Info("草了".to_string())),
        "werwrwrwerwrweerwr".to_string(),
    )
    .to_buf();
    println!("{:?}", KikFrame::from_buf(bytes_mut).unwrap());
}

#[test]
fn empty_frame_returns_none() {
    assert!(KikFrame::from_buf(BytesMut::new()).is_none());
}

#[test]
fn short_resp_extra_returns_none() {
    let mut bys = BytesMut::new();
    bys.put_u8(13);
    bys.put_u32(8);
    bys.put_slice(&[1, 2]);

    assert!(KikFrame::from_buf(bys).is_none());
}

#[test]
fn short_data_frame_returns_none() {
    let mut bys = BytesMut::new();
    bys.put_u8(12);
    bys.put_u8(1);

    assert!(KikFrame::from_buf(bys).is_none());
}
