use crate::ctrl_resp::CmdResp;
use bytes::{Buf, BufMut, BytesMut};
use common::protocol::{BufSerializable, ReqCmd};

const MAX_DATA_ID_BYTES: usize = 128;

#[derive(Debug, Clone)]
pub enum Frame {
    Cmd(ReqCmd),
    Resp(CmdResp),

    Data(String, BytesMut), //数据传输的data帧

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
}
