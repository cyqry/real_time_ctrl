use crate::command::Command;
use crate::generated::encrypted_strings::{CMD_OPTIONS_TIMEOUT_FALSE, CMD_OPTIONS_TIMEOUT_TRUE};
use crate::message::kik_frame::KikFrame;
use bytes::{Buf, BufMut, BytesMut};
use serde::{Deserialize, Serialize};

use crate::hidden;

pub trait BufSerializable {
    fn to_buf(&self) -> BytesMut;
    fn from_buf(bys: BytesMut) -> Option<Self>
    where
        Self: Sized;
}

pub const MAX_CORRELATION_ID_BYTES: usize = 128;
const MAX_COMMAND_OPTIONS_BYTES: usize = 4096;

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
        let id_len = self.id.len();
        let mut bytes_mut = BytesMut::with_capacity(id_len);
        bytes_mut.put_u32(id_len as u32);
        bytes_mut.put_slice(self.id.as_bytes());
        // CmdOptions 当前只有一个 bool；手工生成与 serde_json 相同的稳定字节，
        // 避免在无失败返回值的历史编码 trait 中引入序列化 panic。
        let cop_json = if self.cmd_options.timeout {
            CMD_OPTIONS_TIMEOUT_TRUE()
        } else {
            CMD_OPTIONS_TIMEOUT_FALSE()
        };
        let json_len = cop_json.len();
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
        if id_len == 0 || id_len as usize > MAX_CORRELATION_ID_BYTES || bys.len() < id_len as usize
        {
            return None;
        }
        let id = String::from_utf8(bys.split_to(id_len as usize).to_vec()).ok()?;
        if bys.len() < 4 {
            return None;
        }
        let json_len = bys.get_u32();
        if json_len == 0
            || json_len as usize > MAX_COMMAND_OPTIONS_BYTES
            || bys.len() < json_len as usize
        {
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
        panic!("{}", hidden!("要传输的数据太大"))
    }
    let mut bytes_mut = BytesMut::with_capacity(bts.len() + 4);
    bytes_mut.put_slice(&(bts.len() as u32).to_be_bytes());
    bytes_mut.put(bts);
    bytes_mut
}

pub fn transfer_b_encode(bts: &[u8], start: usize, end: usize) -> BytesMut {
    let len = end - start;
    if len > u32::MAX as usize {
        panic!("{}", hidden!("要传输的数据太大"))
    }
    let mut bytes_mut = BytesMut::with_capacity(len + 4);
    bytes_mut.put_slice(&(len as u32).to_be_bytes());
    bytes_mut.put_slice(&bts[start..end]);
    bytes_mut
}

pub fn transfer_encode_frame(frame: impl BufSerializable) -> BytesMut {
    let bytes_mut = frame.to_buf();
    transfer_encode(bytes_mut)
}

/// 一次分配完成“长度前缀 + 数据帧头 + payload”编码。
///
/// 文件分片是数据面的热路径。先构造业务帧、再套长度前缀会让大块 payload
/// 在同一进程内被重复复制；该函数把每一跳压缩为一次连续内存复制。
pub fn transfer_encode_data_frame(
    frame_code: u8,
    data_id: &str,
    data: &[u8],
    max_id_len: usize,
) -> std::io::Result<BytesMut> {
    let id = data_id.as_bytes();
    if id.is_empty() || id.len() > max_id_len || id.len() > u32::MAX as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            hidden!("数据帧 ID 长度非法"),
        ));
    }
    let frame_len = 1usize
        .checked_add(4)
        .and_then(|len| len.checked_add(id.len()))
        .and_then(|len| len.checked_add(data.len()))
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, hidden!("数据帧长度溢出"))
        })?;
    if frame_len > u32::MAX as usize {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            hidden!("数据帧超过 u32 长度上限"),
        ));
    }

    let mut encoded = BytesMut::with_capacity(4 + frame_len);
    encoded.put_u32(frame_len as u32);
    encoded.put_u8(frame_code);
    encoded.put_u32(id.len() as u32);
    encoded.put_slice(id);
    encoded.put_slice(data);
    Ok(encoded)
}

pub fn kik_ping() -> BytesMut {
    transfer_encode_frame(KikFrame::Ping)
}

pub fn kik_pong() -> BytesMut {
    transfer_encode_frame(KikFrame::Pong)
}

#[test]
pub fn test() {
    use crate::command::CtrlCommand;
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

#[cfg(test)]
mod data_frame_tests {
    use super::*;

    #[test]
    fn contiguous_encoder_keeps_existing_wire_format() {
        let payload = b"payload";
        let mut encoded =
            transfer_encode_data_frame(12, "data-id", payload, MAX_CORRELATION_ID_BYTES).unwrap();

        assert_eq!(encoded.get_u32() as usize, encoded.len());
        match KikFrame::from_buf(encoded) {
            Some(KikFrame::Data(id, data)) => {
                assert_eq!(id, "data-id");
                assert_eq!(data.as_ref(), payload);
            }
            _ => panic!("unexpected frame"),
        }
    }
}
