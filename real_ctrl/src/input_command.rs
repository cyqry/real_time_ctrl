use anyhow::anyhow;
use common::command::LocalCommand::LocalExit;
use common::command::SysCommand::*;
use common::command::{Command, CtrlCommand, LocalCommand, SysCommand};
use std::str::FromStr;

#[derive(Debug, Clone)]
pub enum InputCommand {
    Sys(SysCommand),
    Local(LocalCommand),
    Ctrl(InputCtrlCommand),
    Exec(String),
}

#[derive(Clone, Debug)]
pub enum InputCtrlCommand {
    GetFile(String, String),
    GetBigFile(String, String),
    SetFile(String, String),
    SetBigFile(String, String),
    Ls(String),
    Screen(String),
}

#[cfg(target_os = "windows")]
static DEFAULT_SCREEN_PATH: &str = "D:\\MyTest\\1.png";

impl From<InputCtrlCommand> for CtrlCommand {
    fn from(value: InputCtrlCommand) -> Self {
        match value {
            InputCtrlCommand::GetFile(a, b) => CtrlCommand::GetFile(a, "".to_string()),
            InputCtrlCommand::GetBigFile(a, b) => CtrlCommand::GetFile(a, "".to_string()),
            InputCtrlCommand::Ls(s) => CtrlCommand::Ls(s),
            InputCtrlCommand::Screen(s) => CtrlCommand::Screen(s),
            InputCtrlCommand::SetFile(_, _) => {
                unreachable!("不支持")
            }
            InputCtrlCommand::SetBigFile(_, _) => {
                unreachable!("不支持")
            }
        }
    }
}

impl FromStr for InputCommand {
    type Err = anyhow::Error;

    fn from_str(mut s: &str) -> Result<Self, Self::Err> {
        s = s.trim();
        if s.is_empty() {
            panic!("parse empty");
        }
        if s.starts_with("$") {
            let parts: Vec<&str> = s[1..].split_whitespace().collect();

            match parts.as_slice() {
                ["sys_now"] => Ok(InputCommand::Sys(Now)),
                ["sys_list"] => Ok(InputCommand::Sys(List)),
                ["sys_use", value] => {
                    let val = value.trim_matches('"').to_string();
                    Ok(InputCommand::Sys(Use(val)))
                }
                ["local_exit"] => Ok(InputCommand::Local(LocalExit)),
                // todo 对于文件路径，我希望使用""包裹的参数，都对其进行转义，未使用""包裹的参数，无需转义;只有一边有"符号的是错误的语法
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
fn test() {
    println!("{}", (0 as *mut String).is_null()); //true
                                                  // let x = 0x10 as *mut String;
                                                  // unsafe { println!("{}", *x); }
    let parts: Vec<&str> = "etst est".split_ascii_whitespace().collect();
    let c: InputCommand = "$ls sdfsdf -r".parse().unwrap();
    println!("{:?}", c);
    match parts.as_slice() {
        //  _ @ ..  是一种 匹配模式,匹配剩余的元素
        ["etst", s, others @ ..] => {
            println!("{}", others.len()); // 0
        }
        _ => {}
    }
}
