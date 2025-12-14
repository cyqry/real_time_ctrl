use crate::command::CtrlCommand::{GetBigFile, GetFile, Ls, Screen, SetBigFile, SetFile};
use crate::command::LocalCommand::LocalExit;
use crate::command::SysCommand::{List, Now, Use};
use crate::protocol::{BufSerializable, CmdOptions, ReqCmd};
use anyhow::anyhow;
use bytes::{Buf, BufMut, BytesMut};
use std::str::FromStr;
use crate::message::frame::Frame;

#[derive(Debug, Clone)]
pub enum Command {
    Sys(SysCommand),
    Local(LocalCommand),
    Ctrl(CtrlCommand),
    Exec(String),
}

#[derive(Debug, Clone)]
pub enum LocalCommand {
    LocalExit,
}

//todo 增加Any帧，则协议内容由双方协商，服务端不管
#[derive(Debug, Clone)]
pub enum CtrlCommand {
    GetFile(String, String),
    GetBigFile(String, String),
    SetFile(String, String),
    SetBigFile(String, u64, Vec<u8>, String),
    Ls(String),
    Screen(String),
}

//todo Down kik
#[derive(Debug, Clone)]
pub enum SysCommand {
    List,
    Use(String),
    Now,
}

impl BufSerializable for Command {
    fn to_buf(&self) -> BytesMut {
        let mut bytes_mut = BytesMut::new();
        match self {
            Command::Sys(sys) => {
                bytes_mut.put_u8(0);
                match sys {
                    List => {
                        bytes_mut.put_u8(0);
                    }
                    Use(kik) => {
                        bytes_mut.put_u8(1);
                        bytes_mut.put_slice(kik.as_bytes())
                    }
                    SysCommand::Now => {
                        bytes_mut.put_u8(2);
                    }
                }
            }
            Command::Ctrl(c) => {
                bytes_mut.put_u8(1);
                match c {
                    GetFile(src, dst) => {
                        bytes_mut.put_u8(0);
                        bytes_mut.put_u32(src.as_bytes().len() as u32);
                        bytes_mut.put_slice(src.as_bytes());
                        bytes_mut.put_slice(dst.as_bytes());
                    }
                    GetBigFile(src, dst) => {
                        bytes_mut.put_u8(1);
                        bytes_mut.put_u32(src.as_bytes().len() as u32);
                        bytes_mut.put_slice(src.as_bytes());
                        bytes_mut.put_slice(dst.as_bytes());
                    }
                    SetFile(src, dst) => {
                        bytes_mut.put_u8(2);
                        bytes_mut.put_u32(src.as_bytes().len() as u32);
                        bytes_mut.put_slice(src.as_bytes());
                        bytes_mut.put_slice(dst.as_bytes());
                    }
                    SetBigFile(src, size, hash, dst) => {
                        bytes_mut.put_u8(3);
                        bytes_mut.put_u32(src.as_bytes().len() as u32);
                        bytes_mut.put_slice(src.as_bytes());
                        bytes_mut.put_u64(size.clone());
                        bytes_mut.put_u32(hash.len() as u32);
                        bytes_mut.put_slice(hash);
                        bytes_mut.put_slice(dst.as_bytes());
                    }
                    Ls(path) => {
                        bytes_mut.put_u8(4);
                        bytes_mut.put_slice(path.as_bytes());
                    }
                    Screen(path) => {
                        bytes_mut.put_u8(5);
                        bytes_mut.put_slice(path.as_bytes());
                    }
                }
            }
            Command::Exec(e) => {
                bytes_mut.put_u8(2);
                bytes_mut.put_slice(e.as_bytes());
            }
            _ => {
                panic!("不支持转buf的cmd");
            }
        };
        bytes_mut
    }

    fn from_buf(mut bys: BytesMut) -> Option<Self> {
        if bys.is_empty() {
            return None;
        }
        let first_code = bys.get_u8();
        match first_code {
            0 => {
                if bys.is_empty() {
                    return None;
                }
                let second_code = bys.get_u8();
                match second_code {
                    0 => Some(Command::Sys(List)),
                    1 => Some(Command::Sys(Use(String::from_utf8(bys.to_vec()).ok()?))),
                    2 => Some(Command::Sys(SysCommand::Now)),
                    _ => None,
                }
            }
            1 => {
                if bys.is_empty() {
                    return None;
                }
                let second_code = bys.get_u8();
                match second_code {
                    0 => {
                        if bys.len() < 4 {
                            return None;
                        }
                        let src_len = bys.get_u32();
                        if bys.len() < src_len as usize {
                            return None;
                        }
                        let src = bys.split_to(src_len as usize);
                        Some(Command::Ctrl(GetFile(
                            String::from_utf8(src.to_vec()).ok()?,
                            String::from_utf8(bys.to_vec()).ok()?,
                        )))
                    }
                    1 => {
                        if bys.len() < 4 {
                            return None;
                        }
                        let src_len = bys.get_u32();
                        if bys.len() < src_len as usize {
                            return None;
                        }
                        let src = bys.split_to(src_len as usize);
                        Some(Command::Ctrl(GetBigFile(
                            String::from_utf8(src.to_vec()).ok()?,
                            String::from_utf8(bys.to_vec()).ok()?,
                        )))
                    }
                    2 => {
                        if bys.len() < 4 {
                            return None;
                        }
                        let src_len = bys.get_u32();
                        if bys.len() < src_len as usize {
                            return None;
                        }
                        let src = bys.split_to(src_len as usize);
                        Some(Command::Ctrl(SetFile(
                            String::from_utf8(src.to_vec()).ok()?,
                            String::from_utf8(bys.to_vec()).ok()?,
                        )))
                    }
                    3 => {
                        if bys.len() < 4 {
                            return None;
                        }
                        let src_len = bys.get_u32();
                        if bys.len() < src_len as usize {
                            return None;
                        }
                        let target_path =
                            String::from_utf8(bys.split_to(src_len as usize).to_vec()).ok()?;
                        if bys.len() < 8 {
                            return None;
                        }
                        let size = bys.get_u64();
                        if bys.len() < 4 {
                            return None;
                        }
                        let hash_len = bys.get_u32();
                        if bys.len() < hash_len as usize {
                            return None;
                        }
                        let hash = bys.split_to(hash_len as usize).to_vec();
                        Some(Command::Ctrl(SetBigFile(
                            target_path,
                            size,
                            hash,
                            String::from_utf8(bys.to_vec()).ok()?,
                        )))
                    }

                    4 => Some(Command::Ctrl(Ls(String::from_utf8(bys.to_vec()).ok()?))),
                    5 => Some(Command::Ctrl(Screen(String::from_utf8(bys.to_vec()).ok()?))),
                    _ => None,
                }
            }
            2 => Some(Command::Exec(String::from_utf8(bys.to_vec()).ok()?)),
            _ => None,
        }
    }
}

#[test]
fn test() {
    println!("{:?}", Frame::from_buf(
        Frame::Cmd(ReqCmd::new("sfdid".to_string(), CmdOptions::default().with_timeout(false), Command::Ctrl(CtrlCommand::SetBigFile(
            "werwrwerw".to_string(),
            232,
            vec![12, 3, 4, 5, 3, 6, 66, 12],
            "".to_string(),
        ))))

            .to_buf(),
    )
        .unwrap());
}
