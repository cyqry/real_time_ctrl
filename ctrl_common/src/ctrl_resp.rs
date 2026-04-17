use bytes::{Buf, BufMut, BytesMut};
use common::message::kik_resp::KikResp;
use common::protocol::BufSerializable;

#[derive(Debug, Clone)]
pub enum Resp {
    Server(ServerResp),
    Kik(KikResp),
}

#[derive(Clone, Debug)]
pub struct CmdResp {
    cmd_id: String,
    resp: Resp,
}

#[derive(Clone, Debug)]
pub enum ServerResp {
    Success(ServerSuccessResp),
    Error(u8, String),
}

#[derive(Clone, Debug)]
pub enum ServerSuccessResp {
    Info(String),
}

impl CmdResp {
    pub fn new(cmd_id: String, resp: Resp) -> CmdResp {
        CmdResp { cmd_id, resp }
    }
    pub fn get_resp(&self) -> &Resp {
        &self.resp
    }
    
    pub fn get_cmd_id(&self) -> &String {
        &self.cmd_id
    }
}

impl BufSerializable for ServerSuccessResp {
    fn to_buf(&self) -> BytesMut {
        match self {
            ServerSuccessResp::Info(s) => {
                let mut bytes_mut = BytesMut::new();
                bytes_mut.put_u8(0);
                bytes_mut.put_slice(s.as_bytes());
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
            0 => Some(ServerSuccessResp::Info(
                String::from_utf8(bys.to_vec()).ok()?,
            )),
            _ => None,
        }
    }
}

impl BufSerializable for ServerResp {
    fn to_buf(&self) -> BytesMut {
        match self {
            ServerResp::Success(s) => {
                let mut bytes_mut = BytesMut::new();
                bytes_mut.put_u8(0);
                bytes_mut.put(s.to_buf());
                bytes_mut
            }
            ServerResp::Error(code, msg) => {
                let mut bytes_mut = BytesMut::new();
                bytes_mut.put_u8(1);
                bytes_mut.put_u8(*code);
                bytes_mut.put_slice(msg.as_bytes());
                bytes_mut
            }
        }
    }

    fn from_buf(mut bys: BytesMut) -> Option<Self> {
        let code = bys.get_u8();
        match code {
            0 => Some(ServerResp::Success(ServerSuccessResp::from_buf(bys)?)),
            1 => {
                if bys.len() < 1 {
                    return None;
                }
                let err_code = bys.get_u8();
                let err_msg = String::from_utf8(bys.to_vec()).ok()?;
                Some(ServerResp::Error(err_code, err_msg))
            }
            _ => None,
        }
    }
}

impl BufSerializable for Resp {
    fn to_buf(&self) -> BytesMut {
        match self {
            Resp::Server(server_resp) => {
                let mut bytes_mut = BytesMut::new();
                bytes_mut.put_u8(0);
                bytes_mut.put(server_resp.to_buf());
                bytes_mut
            }
            Resp::Kik(kik_resp) => {
                let mut bytes_mut = BytesMut::new();
                bytes_mut.put_u8(1);
                bytes_mut.put(kik_resp.to_buf());
                bytes_mut
            }
        }
    }

    fn from_buf(mut bys: BytesMut) -> Option<Self> {
        let code = bys.get_u8();
        match code {
            0 => Some(Resp::Server(ServerResp::from_buf(bys)?)),
            1 => Some(Resp::Kik(KikResp::from_buf(bys)?)),
            _ => None,
        }
    }
}

impl BufSerializable for CmdResp {
    fn to_buf(&self) -> BytesMut {
        let id_len = self.cmd_id.as_bytes().len();
        let mut bytes_mut = BytesMut::with_capacity(id_len);
        bytes_mut.put_u32(id_len as u32);
        bytes_mut.put_slice(self.cmd_id.as_bytes());
        bytes_mut.put(self.resp.to_buf());
        bytes_mut
    }

    fn from_buf(mut bys: BytesMut) -> Option<Self> {
        if bys.len() < 4 {
            return None;
        }
        let id_len = bys.get_u32();
        if bys.len() < id_len as usize {
            return None;
        }
        let cmd_id = String::from_utf8(bys.split_to(id_len as usize).to_vec()).ok()?;
        let resp = Resp::from_buf(bys)?;
        Some(CmdResp { cmd_id, resp })
    }
}

#[test]
fn test_cmd_resp_serialization() {
    let cmd_resp = CmdResp {
        cmd_id: "cmd123".to_string(),
        resp: Resp::Server(ServerResp::Success(ServerSuccessResp::Info(
            "Command executed successfully".to_string(),
        ))),
    };

    let buf = cmd_resp.to_buf();
    let deserialized_cmd_resp = CmdResp::from_buf(buf).unwrap();

    assert_eq!(cmd_resp.cmd_id, deserialized_cmd_resp.cmd_id);
    match deserialized_cmd_resp.resp {
        Resp::Server(ServerResp::Success(ServerSuccessResp::Info(info))) => {
            assert_eq!(info, "Command executed successfully");
        }
        _ => panic!("Unexpected response type"),
    }
}
