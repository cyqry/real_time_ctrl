use anyhow::anyhow;
use common::command::LocalCommand::LocalExit;
use common::command::SysCommand::*;
use common::command::{CtrlCommand, LocalCommand, SysCommand};
use common::message::kik_cmd_resp_info;
use ctrl_common::cmd_resp_info::{KikInfoVo, KikPresenceVo, SysNow};
use std::str::FromStr;

#[derive(Debug, Clone)]
pub enum InputCommand {
    Sys(SysCommand),
    Local(LocalCommand),
    Ctrl(InputCtrlCommand),
    Exec(String),
}

#[derive(Debug, Clone)]
pub enum InputCtrlCommand {
    GetFile(String, String),
    GetBigFile(String, String),
    SetFile(String, String),
    SetBigFile(String, String),
    Ls(String),
    Screen(String),
}

#[derive(Debug, Clone)]
pub enum RemoteResp {
    Success(RemoteSuccessResp),
    SuccessData(Vec<u8>),
    Error(u32, String),
}

#[derive(Clone, Debug)]
pub enum RemoteSuccessResp {
    Info(String),
    Ls(Vec<kik_cmd_resp_info::Ls>),
    SysList(Vec<KikInfoVo>),
    History(Vec<KikPresenceVo>),
    Now(SysNow),
}

#[cfg(target_os = "windows")]
static DEFAULT_SCREEN_PATH: &str = "D:\\MyTest\\1.png";

impl TryFrom<InputCtrlCommand> for CtrlCommand {
    type Error = anyhow::Error;

    fn try_from(value: InputCtrlCommand) -> Result<Self, Self::Error> {
        match value {
            InputCtrlCommand::GetFile(a, _) => Ok(CtrlCommand::GetFile(a, String::new())),
            InputCtrlCommand::GetBigFile(a, _) => Ok(CtrlCommand::GetBigFile(a, String::new())),
            InputCtrlCommand::Ls(s) => Ok(CtrlCommand::Ls(s)),
            // 保存路径属于 real_ctrl 本地状态，被控端只负责产生截图数据，不应获知该路径。
            InputCtrlCommand::Screen(_) => Ok(CtrlCommand::Screen(String::new())),
            InputCtrlCommand::SetFile(_, _) | InputCtrlCommand::SetBigFile(_, _) => {
                Err(anyhow!("文件上传命令必须先经过本地数据预处理"))
            }
        }
    }
}

impl FromStr for InputCommand {
    type Err = anyhow::Error;

    fn from_str(mut s: &str) -> Result<Self, Self::Err> {
        s = s.trim();
        if s.is_empty() {
            return Err(anyhow!("命令不能为空"));
        }
        if let Some(command_body) = s.strip_prefix('$') {
            let parts: Vec<&str> = command_body.split_whitespace().collect();

            match parts.as_slice() {
                ["sys_now"] => Ok(InputCommand::Sys(Now)),
                ["sys_list"] => Ok(InputCommand::Sys(List)),
                ["sys_history"] => Ok(InputCommand::Sys(History(None))),
                ["sys_history", value] => Ok(InputCommand::Sys(History(Some(
                    value.trim_matches('"').to_string(),
                )))),
                ["sys_use", value] => {
                    let val = value.trim_matches('"').to_string();
                    Ok(InputCommand::Sys(Use(val)))
                }
                ["local_exit"] => Ok(InputCommand::Local(LocalExit)),
                // CLI 按空白切分，带空格路径应通过 HTTP/pipe 结构化 API 传入。
                ["screen", save_path] => {
                    let save_path = save_path.trim_matches('"').to_string();
                    Ok(InputCommand::Ctrl(InputCtrlCommand::Screen(save_path)))
                }
                #[cfg(target_os = "windows")]
                ["screen"] => Ok(InputCommand::Ctrl(InputCtrlCommand::Screen(
                    DEFAULT_SCREEN_PATH.to_string(),
                ))),
                ["getfile", src, "to", dest, ..]
                | ["setfile", src, "to", dest, ..]
                | ["setbigfile", src, "to", dest, ..]
                | ["getbigfile", src, "to", dest, ..] => {
                    let src = src.trim_matches('"').to_string();
                    let dest = dest.trim_matches('"').to_string();

                    if parts[0] == "getfile" {
                        Ok(InputCommand::Ctrl(InputCtrlCommand::GetFile(src, dest)))
                    } else if parts[0] == "setfile" {
                        Ok(InputCommand::Ctrl(InputCtrlCommand::SetFile(src, dest)))
                    } else if parts[0] == "setbigfile" {
                        Ok(InputCommand::Ctrl(InputCtrlCommand::SetBigFile(src, dest)))
                    } else {
                        Ok(InputCommand::Ctrl(InputCtrlCommand::GetBigFile(src, dest)))
                    }
                }
                ["ls", dir, args @ ..] => {
                    let dir = dir.trim_matches('"').to_string();
                    match args.len() {
                        0 => Ok(InputCommand::Ctrl(InputCtrlCommand::Ls(dir))),
                        _ => {
                            //先不做特殊处理
                            let v: Vec<&str> = std::iter::once(dir.as_str())
                                .chain(args.iter().cloned())
                                .collect();
                            Ok(InputCommand::Ctrl(InputCtrlCommand::Ls(v.join(" "))))
                        }
                    }
                }
                _ => unknown(s),
            }
        } else {
            Ok(InputCommand::Exec(s.to_string()))
        }
    }
}

fn unknown<T>(s: &str) -> anyhow::Result<T> {
    Err(anyhow!(format!("Unknown command: {}", s)))
}

#[test]
fn parses_recursive_ls_command() {
    let command: InputCommand = "$ls sdfsdf -r".parse().unwrap();
    assert!(matches!(
        command,
        InputCommand::Ctrl(InputCtrlCommand::Ls(path)) if path == "sdfsdf -r"
    ));
}

#[test]
fn strips_controller_local_paths_before_sending_to_kik() {
    assert!(matches!(
        CtrlCommand::try_from(InputCtrlCommand::GetFile(
            "remote.txt".to_string(),
            "D:/controller/private.txt".to_string(),
        ))
        .unwrap(),
        CtrlCommand::GetFile(_, destination) if destination.is_empty()
    ));
    assert!(matches!(
        CtrlCommand::try_from(InputCtrlCommand::GetBigFile(
            "remote.bin".to_string(),
            "D:/controller/private.bin".to_string(),
        ))
        .unwrap(),
        CtrlCommand::GetBigFile(_, destination) if destination.is_empty()
    ));
    assert!(matches!(
        CtrlCommand::try_from(InputCtrlCommand::Screen(
            "D:/controller/screen.png".to_string(),
        ))
        .unwrap(),
        CtrlCommand::Screen(destination) if destination.is_empty()
    ));
}
