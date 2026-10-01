//! 三端线上命令的语义模型及二进制编解码。
//!
//! `Command` 只表示会跨网络发送的动作。控制端本地退出、控制端本地保存路径等信息不得混入这里，
//! 它们由 `real_ctrl::input_command` 单独维护。各变体中字符串的具体含义见根目录 `协议说明.md`。

use crate::command::CtrlCommand::{GetBigFile, GetFile, Ls, Screen, SetBigFile, SetFile};
use crate::command::SysCommand::{History, List};
use crate::protocol::BufSerializable;
use bytes::{Buf, BufMut, BytesMut};
use serde::{Deserialize, Serialize};

const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_KIK_ID_BYTES: usize = 128;
const MAX_HASH_BYTES: usize = 64;

#[derive(Debug, Clone)]
/// 线上命令的顶层分类。
pub enum Command {
    Sys(SysCommand),
    Ctrl(CtrlCommand),
    Exec(String),
    /// 服务端目录中的命名任务；不接受程序路径和临时参数。
    RunTask(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// 只在调用端进程内生效的命令，不参与 `BufSerializable`。
pub enum LocalCommand {
    LocalExit,
    /// 选择状态属于客户端；此操作不得编码成服务端选择命令。
    LocalUse(String),
    /// 查看本地保存的目标快照，不代表设备此刻仍在线。
    LocalNow,
}

#[derive(Debug, Clone)]
/// 需要由 Kik 执行的文件、目录和屏幕命令。
///
/// 文件传输变体中的第二个字符串并不总是“路径”：下载时它是数据 ID，上传时第一个字符串是数据 ID。
/// 这种历史元组布局容易误读，新增代码应优先在调用点用清晰变量名解构。
pub enum CtrlCommand {
    GetFile(String, String),
    GetBigFile(String, String),
    SetFile(String, String),
    SetBigFile(String, u64, Vec<u8>, String),
    Ls(String),
    Screen(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SysCommand {
    List,
    /// 查询当前服务进程记录的最近上下线状态；`None` 返回全部有界历史。
    History(Option<String>),
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
                    History(kik_id) => {
                        bytes_mut.put_u8(3);
                        match kik_id {
                            Some(kik_id) => {
                                bytes_mut.put_u8(1);
                                bytes_mut.put_slice(kik_id.as_bytes());
                            }
                            None => bytes_mut.put_u8(0),
                        }
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
                    SetBigFile(src, size, hash, dst) => {
                        bytes_mut.put_u8(3);
                        bytes_mut.put_u32(src.len() as u32);
                        bytes_mut.put_slice(src.as_bytes());
                        bytes_mut.put_u64(*size);
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
            Command::RunTask(name) => {
                bytes_mut.put_u8(3);
                bytes_mut.put_slice(name.as_bytes());
            }
        };
        bytes_mut
    }

    fn from_buf(mut bys: BytesMut) -> Option<Self> {
        if bys.is_empty() || bys.len() > MAX_COMMAND_BYTES {
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
                    0 if bys.is_empty() => Some(Command::Sys(List)),
                    // 旧服务端选择命令的 1/2 编号永久保留；拒绝旧请求，不能复用为其他操作。
                    1 | 2 => None,
                    3 => {
                        if bys.is_empty() {
                            return None;
                        }
                        match bys.get_u8() {
                            0 if bys.is_empty() => Some(Command::Sys(History(None))),
                            1 if !bys.is_empty() && bys.len() <= MAX_KIK_ID_BYTES => Some(
                                Command::Sys(History(Some(String::from_utf8(bys.to_vec()).ok()?))),
                            ),
                            _ => None,
                        }
                    }
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
                        if hash_len == 0
                            || hash_len as usize > MAX_HASH_BYTES
                            || bys.len() < hash_len as usize
                        {
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
            2 if !bys.is_empty() => Some(Command::Exec(String::from_utf8(bys.to_vec()).ok()?)),
            3 => {
                let name = String::from_utf8(bys.to_vec()).ok()?;
                crate::task::valid_task_name(&name).then_some(Command::RunTask(name))
            }
            _ => None,
        }
    }
}

#[test]
fn test() {
    use crate::protocol::{CmdOptions, ReqCmd};
    println!(
        "{:?}",
        ReqCmd::from_buf(
            ReqCmd::new(
                "sfdid".to_string(),
                CmdOptions::default().with_timeout(false),
                Command::Ctrl(CtrlCommand::SetBigFile(
                    "werwrwerw".to_string(),
                    232,
                    vec![12, 3, 4, 5, 3, 6, 66, 12],
                    "".to_string(),
                ))
            )
            .to_buf()
        )
        .unwrap()
    );
}

#[test]
fn sys_history_round_trip_and_rejects_invalid_presence_flag() {
    for expected in [None, Some("kik-1".to_string())] {
        let encoded = Command::Sys(SysCommand::History(expected.clone())).to_buf();
        let decoded = Command::from_buf(encoded).unwrap();
        match decoded {
            Command::Sys(SysCommand::History(actual)) => assert_eq!(actual, expected),
            _ => panic!("系统历史命令往返类型错误"),
        }
    }

    let mut invalid = BytesMut::from(&[0_u8, 3, 2][..]);
    assert!(Command::from_buf(invalid.split()).is_none());
}

#[test]
fn removed_server_selection_opcodes_are_never_accepted() {
    for old_command in [&[0_u8, 1, b'a'][..], &[0_u8, 2][..]] {
        assert!(Command::from_buf(BytesMut::from(old_command)).is_none());
    }
}

#[test]
fn run_task_has_its_own_opcode_and_rejects_non_names() {
    let encoded = Command::RunTask("task_A-1".into()).to_buf();
    assert_eq!(encoded[0], 3);
    assert!(
        matches!(Command::from_buf(encoded), Some(Command::RunTask(name)) if name == "task_A-1")
    );
    for name in [
        "",
        "../task",
        "task.exe",
        "task arg",
        "task\0hidden",
        "任务",
    ] {
        assert!(Command::from_buf(Command::RunTask(name.into()).to_buf()).is_none());
    }
    assert!(Command::from_buf(Command::RunTask("a".repeat(65)).to_buf()).is_none());
    assert!(Command::from_buf(Command::RunTask("a".repeat(64)).to_buf()).is_some());
}
