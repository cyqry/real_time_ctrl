use crate::command::CtrlCommand::{GetBigFile, GetFile, Ls, Screen, SetFile};
use crate::command::LocalCommand::LocalExit;
use crate::command::SysCommand::{List, Now, Use};
use crate::protocol::BufSerializable;
use bytes::{Buf, BufMut, BytesMut};
use std::str::FromStr;
use anyhow::anyhow;

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
    Ls(String),
    Screen(String),
}

#[cfg(target_os = "windows")]
static DEFAULT_SCREEN_PATH: &str = "D:\\MyTest\\1.png";

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
                        bytes_mut.put_u32(src.len() as u32);
                        bytes_mut.put_slice(src.as_bytes());
                        bytes_mut.put_slice(dst.as_bytes());
                    }
                    GetBigFile(src, dst) => {
                        bytes_mut.put_u8(1);
                        bytes_mut.put_u32(src.len() as u32);
                        bytes_mut.put_slice(src.as_bytes());
                        bytes_mut.put_slice(dst.as_bytes());
                    }
                    SetFile(src, dst) => {
                        bytes_mut.put_u8(2);
                        bytes_mut.put_u32(src.len() as u32);
                        bytes_mut.put_slice(src.as_bytes());
                        bytes_mut.put_slice(dst.as_bytes());
                    }
                    Ls(path) => {
                        bytes_mut.put_u8(3);
                        bytes_mut.put_slice(path.as_bytes());
                    }

                    Screen(path) => {
                        bytes_mut.put_u8(4);
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
                    3 => Some(Command::Ctrl(Ls(String::from_utf8(bys.to_vec()).ok()?))),
                    4 => Some(Command::Ctrl(Screen(String::from_utf8(bys.to_vec()).ok()?))),
                    _ => None,
                }
            }
            2 => Some(Command::Exec(String::from_utf8(bys.to_vec()).ok()?)),
            _ => None,
        }
    }
}

#[cfg(target_os = "windows")]
impl FromStr for Command {
    type Err = anyhow::Error;

    fn from_str(mut s: &str) -> Result<Self, Self::Err> {
        s = s.trim();
        if s.is_empty() {
            panic!("parse empty");
        }
        if s.starts_with("$") {
            let parts: Vec<&str> = s[1..].split_whitespace().collect();

            match parts.as_slice() {
                ["sys_now"] => Ok(Command::Sys(Now)),
                ["sys_list"] => Ok(Command::Sys(List)),
                ["sys_use", value] => {
                    let val = value.trim_matches('"').to_string();
                    Ok(Command::Sys(Use(val)))
                }
                ["local_exit"] => Ok(Command::Local(LocalExit)),
                // todo 对于文件路径，我希望使用""包裹的参数，都对其进行转义，未使用""包裹的参数，无需转义;只有一边有"符号的是错误的语法
                ["screen", save_path] => {
                    let save_path = save_path.trim_matches('"').to_string();
                    Ok(Command::Ctrl(Screen(save_path)))
                }
                ["screen"] => {
                    Ok(Command::Ctrl(Screen(DEFAULT_SCREEN_PATH.to_string())))
                }
                ["getfile", src, "to", dest, ..]
                | ["setfile", src, "to", dest, ..]
                | ["getbigfile", src, "to", dest, ..] => {
                    let src = src.trim_matches('"').to_string();
                    let dest = dest.trim_matches('"').to_string();

                    if parts[0] == "getfile" {
                        Ok(Command::Ctrl(GetFile(src, dest)))
                    } else if parts[0] == "setfile" {
                        Ok(Command::Ctrl(SetFile(src, dest)))
                    } else {
                        Ok(Command::Ctrl(GetBigFile(src, dest)))
                    }
                }
                ["ls", dir, args @ .. ] => {
                    let dir = dir.trim_matches('"').to_string();
                    match args.len() {
                        0 => {
                            Ok(Command::Ctrl(Ls(dir)))
                        }
                        _ => {
                            //先不做特殊处理
                            let v: Vec<&str> = std::iter::once(dir.as_str()).chain(args.iter().cloned()).collect();
                            Ok(Command::Ctrl(Ls(v.join(" "))))
                        }
                    }
                }
                _ => unknown(s),
            }
        } else {
            Ok(Command::Exec(s.to_string()))
        }
    }
}

fn unknown<T>(s: &str) -> anyhow::Result<T> {
    Err(anyhow!(format!("Unknown command: {}", s)))
}

#[test]
fn test() {
    println!("{}", (0 as *mut String).is_null()); //true
    // let x = 0x10 as *mut String;
    // unsafe { println!("{}", *x); }
    let parts: Vec<&str> = "etst est".split_ascii_whitespace().collect();
    let c: Command = "$ls sdfsdf -r".parse().unwrap();
    println!("{:?}", c);
    match parts.as_slice() {
        //  _ @ ..  是一种 匹配模式,匹配剩余的元素
        ["etst", s, others @ ..] => {
            println!("{}", others.len()); // 0
        }
        _ => {}
    }
}
