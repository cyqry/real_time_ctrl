use crate::context::Context;
use crate::{cmd_util, screen};
use anyhow::anyhow;
use common::command::{Command, CtrlCommand};
use common::file_util;
use common::message::dok::Dok;
use common::message::dok::Dok::FilePart;
use common::message::kik_cmd_resp_info;
use common::message::kik_resp::{kik_error, kik_success_data_id, kik_success_info, KikResp};
use common::protocol::BufSerializable;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio_util::either::Either;
use uuid::Uuid;

pub async fn run(context: &Context, cmd: Command) -> KikResp {
    match cmd {
        Command::Ctrl(c) => {
            let resp = match c {
                CtrlCommand::GetFile(file_path, _) => match file_util::read_file(file_path).await {
                    Ok(v) => match context.find_and_send_data(&v).await {
                        Ok(data_id) => {
                            return kik_success_data_id(data_id);
                        }
                        Err(e) => kik_error(format!("Kik发送数据失败,error:{:?}", e)),
                    },
                    Err(e) => kik_error(format!("Kik读取文件失败:{}", e)),
                },
                CtrlCommand::GetBigFile(file_path, _) => {
                    match do_get_big_file(context, file_path).await {
                        Either::Left(info) => kik_success_info(info),
                        Either::Right(data_id) => return kik_success_data_id(data_id),
                    }
                }
                CtrlCommand::SetBigFile(data_id, total, hash, save_path) => {
                    match set_big_file(context, data_id, total, hash, save_path.clone()).await {
                        Ok(_) => kik_success_info(format!("保存大文件至Kik:{}成功", save_path)),
                        Err(e) => kik_success_info(format!(
                            "保存大文件至Kik:{}失败,error:{}",
                            save_path, e
                        )),
                    }
                }
                CtrlCommand::SetFile(data_id, save_path) => {
                    //recv data
                    match context.read_data(data_id).await {
                        Ok(data) => {
                            //save_path
                            match file_util::save_file(save_path.as_str(), &data).await {
                                Ok(_) => {
                                    kik_success_info(format!("保存文件至Kik:{}成功", save_path))
                                }
                                Err(e) => kik_error(format!(
                                    "保存文件至Kik:{}失败,error:{}",
                                    save_path, e
                                )),
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
                    .await
                    .and_then(|v| {
                        Ok(serde_json::to_string(
                            &v.into_iter()
                                .map(|(filename, is_file, size, created_date, modified_date)| {
                                    kik_cmd_resp_info::Ls {
                                        size,
                                        filename,
                                        is_file,
                                        created_date,
                                        modified_date,
                                    }
                                })
                                .collect::<Vec<kik_cmd_resp_info::Ls>>(),
                        )?)
                    }) {
                        Ok(json) => kik_success_info(json),
                        Err(e) => kik_error(e.to_string()),
                    }
                }
                CtrlCommand::Screen(_) => {
                    let request = screen::CaptureRequest::png(screen::PngProfile::Balanced);
                    match screen::capture_screen(request).await {
                        Ok(v) => match context.find_and_send_data(&v).await {
                            Ok(data_id) => {
                                return kik_success_data_id(data_id);
                            }
                            Err(e) => kik_error(format!("Kik发送数据失败,error:{:?}", e)),
                        },
                        Err(e) => kik_error(format!("Kik截屏失败,error:{:?}", e)),
                    }
                }
            };
            resp
        }
        Command::Exec(s) => {
            if !cfg!(feature = "dangerous-exec") {
                return kik_error(
                    "当前 ctrl_kik 构建未启用 dangerous-exec，拒绝任意命令执行".to_string(),
                );
            }
            // let v: Vec<String> = s.trim().split_whitespace().map(|x| x.to_string()).collect();

            match cmd_util::cmd_exec_line(s.as_str(), false, true).await {
                Ok(res) => kik_success_info(res),
                Err(e) => kik_error(format!("cmd exec error:{}", e)),
            }
        }
        _ => kik_error("暂不支持该类型消息".to_string()),
    }
}

async fn do_get_big_file(context: &Context, file_path: String) -> Either<String, String> {
    let file_size = match file_util::get_file_size(file_path.as_str()).await {
        Ok(size) => size,
        Err(error) => return Either::Left(format!("获取文件失败,error:{}", error)),
    };
    if file_size > 1024 * 1024 * 1024 {
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
    const MAX_BIG_FILE_BYTES: u64 = 1024 * 1024 * 1024;
    if total > MAX_BIG_FILE_BYTES {
        return Err(anyhow!("暂不支持 1 GiB 以上的大文件"));
    }
    if hash.len() != 32 {
        return Err(anyhow!("SHA-256 长度必须为 32 bytes"));
    }
    let mut sum = 0;
    let destination = PathBuf::from(&save_path);
    let file_name = destination
        .file_name()
        .ok_or(anyhow!("获取文件名失败"))?
        .to_string_lossy();
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).await?;
    // 临时文件必须与目标文件位于同一卷，最终才能原子替换且不提前破坏旧文件。
    let temp_file_path = parent.join(format!(".{}.{}.rtc-part", file_name, Uuid::new_v4()));
    let temp_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_file_path)
        .await?;
    drop(temp_file);

    while sum < total {
        let data = context.read_data(data_id.clone()).await?;
        let dok = Dok::from_buf(data).ok_or(anyhow!("大文件数据格式错误!"))?;
        if let FilePart(start, end, data) = dok {
            sum += data.len() as u64;
            file_util::write_range_file(&temp_file_path, start, end, data).await?;
            if sum == total {
                file_util::set_file_size(&temp_file_path, total).await?;
                break;
            } else if sum > total {
                return Err(anyhow!("获取大文件数据错误!!!"));
            }
        } else {
            return Err(anyhow!("大文件保存失败"));
        }
    }
    if !hash.eq(&file_util::compute_hash(temp_file_path.to_string_lossy()).await?) {
        return Err(anyhow!("hash校验失败，数据错误"));
    }
    replace_file(&temp_file_path, &destination).await?;
    Ok(())
}

async fn replace_file(source: &Path, destination: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = source.to_path_buf();
    let destination = destination.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let source_wide = source
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let destination_wide = destination
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        unsafe {
            MoveFileExW(
                PCWSTR(source_wide.as_ptr()),
                PCWSTR(destination_wide.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(|error| anyhow!("原子替换目标文件失败: {error}"))
    })
    .await
    .map_err(|error| anyhow!("文件替换任务失败: {error}"))??;
    Ok(())
}
