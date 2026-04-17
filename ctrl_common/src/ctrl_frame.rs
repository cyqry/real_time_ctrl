use bytes::{Buf, BufMut, BytesMut};
use common::command::Command;
use common::kik_info::KikInfo;
use common::message::kik_resp::{KikResp};
use common::protocol::{BufSerializable, ReqCmd};
use crate::ctrl_resp::CmdResp;

#[derive(Debug, Clone)]
pub enum Frame {
    Cmd(ReqCmd),
    Resp(CmdResp),

    Data(String, BytesMut), //数据传输的data帧

    //todo 增加ping pong负载，如果存在负载则需响应，用于探活
    Ping,
    Pong,
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
        }
    }

    fn from_buf(mut bys: BytesMut) -> Option<Self> {
        let code = bys.get_u8();
        match code {
            11 => Some(Frame::Cmd(ReqCmd::from_buf(bys)?)),
            12 => Some(Frame::Resp(CmdResp::from_buf(bys)?)),
            13 => {
                let id_len = bys.get_u32() as usize;
                if bys.len() < id_len {
                    return None;
                }
                let id_bys = bys.split_to(id_len);
                let data_id = String::from_utf8(id_bys.to_vec()).ok()?;
                Some(Frame::Data(data_id, bys))
            }
            14 => Some(Frame::Ping),
            15 => Some(Frame::Pong),
            _ => None,
        }
    }
}
