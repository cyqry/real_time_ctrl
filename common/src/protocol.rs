use crate::command::{Command, CtrlCommand};
use crate::message::kik_frame::KikFrame;
use bytes::{Buf, BufMut, BytesMut};
use serde::{Deserialize, Serialize};

pub trait BufSerializable {
    fn to_buf(&self) -> BytesMut;
    fn from_buf(bys: BytesMut) -> Option<Self>
    where
        Self: Sized;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CmdOptions {
    timeout: bool,
}

#[derive(Debug, Clone)]
pub struct ReqCmd {
    id: String,
    cmd_options: CmdOptions,
    cmd: Command,
}

pub enum ErrCode {
    EXCEPTION = 1,
}

impl Default for CmdOptions {
    fn default() -> Self {
        Self { timeout: true }
    }
}
impl CmdOptions {
    pub fn timeout(&self) -> bool {
        self.timeout
    }
    pub fn with_timeout(mut self, timeout: bool) -> Self {
        self.timeout = timeout;
        self
    }
}

impl ReqCmd {
    pub fn new(id: String, cmd_options: CmdOptions, cmd: Command) -> Self {
        ReqCmd {
            id,
            cmd_options,
            cmd,
        }
    }
    pub fn get_id(&self) -> &str {
        &self.id
    }
    pub fn get_cmd_options(&self) -> &CmdOptions {
        &self.cmd_options
    }
    pub fn get_cmd(&self) -> &Command {
        &self.cmd
    }

    pub fn split(self) -> (String, CmdOptions, Command) {
        (self.id, self.cmd_options, self.cmd)
    }
}

impl BufSerializable for ReqCmd {
    fn to_buf(&self) -> BytesMut {
        let id_len = self.id.as_bytes().len();
        let mut bytes_mut = BytesMut::with_capacity(id_len);
        bytes_mut.put_u32(id_len as u32);
        bytes_mut.put_slice(self.id.as_bytes());
        let cop_json = serde_json::to_string(&self.cmd_options).unwrap();
        let json_len = cop_json.as_bytes().len();
        bytes_mut.put_u32(json_len as u32);
        bytes_mut.put_slice(cop_json.as_bytes());
        bytes_mut.put(self.cmd.to_buf());
        bytes_mut
    }

    fn from_buf(mut bys: BytesMut) -> Option<Self>
    where
        Self: Sized,
    {
        if bys.len() < 4 {
            return None;
        }
        let id_len = bys.get_u32();
        if bys.len() < id_len as usize {
            return None;
        }
        let id = String::from_utf8(bys.split_to(id_len as usize).to_vec()).ok()?;
        if bys.len() < 4 {
            return None;
        }
        let json_len = bys.get_u32();
        if bys.len() < json_len as usize {
            return None;
        }
        let cmd_options = serde_json::from_str::<CmdOptions>(
            String::from_utf8(bys.split_to(json_len as usize).to_vec())
                .ok()?
                .as_str(),
        )
        .ok()?;
        let cmd = Command::from_buf(bys)?;
        Some(ReqCmd {
            id,
            cmd_options,
            cmd,
        })
    }
}




//对应 ltc解码器 data长度 data内容的格式
pub fn transfer_encode(bts: BytesMut) -> BytesMut {
    if bts.len() > u32::MAX as usize {
        panic!("要传输的数据太大")
    }
    let mut bytes_mut = BytesMut::with_capacity(bts.len() + 4);
    bytes_mut.put_slice(&(bts.len() as u32).to_be_bytes());
    bytes_mut.put(bts);
    bytes_mut
}

pub fn transfer_b_encode(bts: &[u8], start: usize, end: usize) -> BytesMut {
    let len = end - start;
    if len > u32::MAX as usize {
        panic!("要传输的数据太大")
    }
    let mut bytes_mut = BytesMut::with_capacity((len + 4));
    bytes_mut.put_slice(&(len as u32).to_be_bytes());
    bytes_mut.put_slice(&bts[start..end]);
    bytes_mut
}



pub fn transfer_encode_frame(frame: impl BufSerializable) -> BytesMut {
    let bytes_mut = frame.to_buf();
    transfer_encode(bytes_mut)
}

pub fn kik_ping() -> BytesMut {
    transfer_encode_frame(KikFrame::Ping)
}


pub fn kik_pong() -> BytesMut {
    transfer_encode_frame(KikFrame::Pong)
}

#[test]
pub fn test() {
    println!(
        "{:?}",
        ReqCmd::from_buf(
            ReqCmd::new(
                "sdfs".to_string(),
                CmdOptions::default().with_timeout(true),
                Command::Ctrl(CtrlCommand::GetFile(
                    "wrew".to_string(),
                    "wrwer/werw".to_string()
                ))
            )
            .to_buf()
        )
        .unwrap()
    );
}
