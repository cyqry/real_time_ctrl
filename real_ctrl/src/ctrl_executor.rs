use crate::context::{id, Context};
use crate::input_command::{InputCtrlCommand, RemoteResp, RemoteSuccessResp};
use anyhow::{anyhow, Context as AnyhowContext};
use common::command::{Command, CtrlCommand};
use common::file_util;
use common::message::dok::{Dok, ErrCode};
use common::message::kik_cmd_resp_info;
use common::message::kik_resp::{ClientSuccessResp, KikResp};
use common::protocol::{BufSerializable, CmdOptions, ReqCmd};
use ctrl_common::ctrl_resp::{Resp, ServerResp, ServerSuccessResp};
use std::path::Path;
use std::path::PathBuf;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;
use tokio::task::{JoinHandle, JoinSet};
use tokio_stream::StreamExt;
use uuid::Uuid;

pub async fn execute(
    context: &Context,
    input_ctrl_cmd: InputCtrlCommand,
    origin_data: bool,
) -> anyhow::Result<RemoteResp> {
    if origin_data
        && matches!(
            &input_ctrl_cmd,
            InputCtrlCommand::GetBigFile(_, local_path) if local_path.is_empty()
        )
    {
        return Err(anyhow!(
            "大文件下载必须提供 local_path，开放 API 不允许把整文件聚合进响应内存"
        ));
    }
    let return_raw_data = origin_data
        && match &input_ctrl_cmd {
            InputCtrlCommand::Screen(_) => true,
            InputCtrlCommand::GetFile(_, local_path) => local_path.is_empty(),
            _ => false,
        };
    let (cmd, cmd_options, pending_transfer) = process_cmd(context, input_ctrl_cmd.clone()).await?;
    let (transfer_start, transfer_task) = match pending_transfer {
        Some(transfer) => (Some(transfer.start), Some(transfer.task)),
        None => (None, None),
    };

    let req_cmd = ReqCmd::new(id(), cmd_options, Command::Ctrl(cmd.clone()));
    let response = match context.request_after_send(&req_cmd, transfer_start).await {
        Ok(response) => response,
        Err(error) => {
            if let Some(task) = transfer_task {
                task.abort();
            }
            return Err(error);
        }
    };
    let response_succeeded = matches!(
        response.get_resp(),
        Resp::Kik(KikResp::Success(_))
            | Resp::Server(ServerResp::Success(ServerSuccessResp::Info(_)))
    );
    if let Some(task) = transfer_task {
        if response_succeeded {
            task.await
                .map_err(|error| anyhow!("文件发送任务异常结束: {error}"))??;
        } else {
            // 服务端提前拒绝命令时立即停止生产数据，避免后台任务污染下一条命令。
            task.abort();
        }
    }

    match response.get_resp() {
        Resp::Kik(KikResp::Success(ClientSuccessResp::Info(info)))
        | Resp::Server(ServerResp::Success(ServerSuccessResp::Info(info))) => {
            //解析响应的info信息
            Ok(RemoteResp::Success(to_remote_resp(
                input_ctrl_cmd,
                info.to_string(),
            )?))
        }
        Resp::Kik(KikResp::Error(err_code, info)) => {
            Ok(RemoteResp::Error(*err_code as u32, info.to_string()))
        }
        Resp::Server(ServerResp::Error(err_code, info)) => {
            Ok(RemoteResp::Error(*err_code as u32, info.to_string()))
        }
        Resp::Kik(KikResp::Success(ClientSuccessResp::DataId(data_id))) => {
            if return_raw_data {
                let result = context.wait_data(data_id.as_str()).await;
                context.finish_data_route(data_id).await;
                let v = result.context("获取数据失败")?;
                Ok(RemoteResp::SuccessData(v.to_vec()))
            } else {
                let result = process_ctrl_cmd_data_id_resp(context, input_ctrl_cmd, data_id).await;
                context.finish_data_route(data_id).await;
                let ok_info = result?;
                Ok(RemoteResp::Success(RemoteSuccessResp::Info(ok_info)))
            }
        }
        Resp::Kik(KikResp::Success(ClientSuccessResp::BigFile {
            data_id,
            total,
            hash,
        })) => match input_ctrl_cmd {
            InputCtrlCommand::GetBigFile(_, save_path) => {
                let result = receive_big_file(context, data_id, &save_path, *total, hash).await;
                context.finish_data_route(data_id).await;
                result?;
                Ok(RemoteResp::Success(RemoteSuccessResp::Info(format!(
                    "保存大文件至:{save_path}"
                ))))
            }
            _ => Err(anyhow!("当前命令收到不匹配的大文件控制响应")),
        },
    }
}

struct PendingTransfer {
    start: oneshot::Sender<()>,
    task: JoinHandle<anyhow::Result<()>>,
}

//根据请求类型反序列化响应info
fn to_remote_resp(cmd: InputCtrlCommand, info: String) -> anyhow::Result<RemoteSuccessResp> {
    let res = match cmd {
        InputCtrlCommand::GetFile(_, _)
        | InputCtrlCommand::GetBigFile(_, _)
        | InputCtrlCommand::SetFile(_, _)
        | InputCtrlCommand::SetBigFile(_, _) => RemoteSuccessResp::Info(info),
        InputCtrlCommand::Ls(_) => RemoteSuccessResp::Ls(
            serde_json::from_str::<Vec<kik_cmd_resp_info::Ls>>(info.as_str())
                .context(format!("json解析失败，原info:{}", info))?,
        ),
        _ => return Err(anyhow!("控制命令返回了不支持的文本响应")),
    };
    Ok(res)
}

async fn process_cmd(
    context: &Context,
    input_ctrl_cmd: InputCtrlCommand,
) -> anyhow::Result<(CtrlCommand, CmdOptions, Option<PendingTransfer>)> {
    let prepared = match input_ctrl_cmd {
        InputCtrlCommand::SetFile(file_path, target_path) => {
            do_set_file(context, file_path, target_path).await?
        }
        InputCtrlCommand::SetBigFile(file_path, target_path) => {
            do_set_big_file(context, file_path, target_path).await?
        }
        // 大文件下载只需要延长命令时限；线上的 CtrlCommand 仍必须通过统一转换，
        // 以剥离仅属于 real_ctrl 的本地保存路径。
        get @ InputCtrlCommand::GetBigFile(_, _) => (
            CtrlCommand::try_from(get)?,
            CmdOptions::default().with_timeout(false),
            None,
        ),
        icc => (CtrlCommand::try_from(icc)?, CmdOptions::default(), None),
    };
    Ok(prepared)
}

//自动处理逻辑
async fn process_ctrl_cmd_data_id_resp(
    context: &Context,
    input_ctrl_cmd: InputCtrlCommand,
    data_id: &str,
) -> anyhow::Result<String> {
    let ok_info = match input_ctrl_cmd {
        InputCtrlCommand::GetFile(_, save_path) => {
            //get data
            match context.wait_data(data_id).await {
                Ok(data) => match file_util::save_file(save_path.as_str(), &data).await {
                    Ok(_) => Ok(format!("保存文件至:{}", save_path)),
                    Err(e) => Err(anyhow!(format!("保存文件至:{}失败,error:{}", save_path, e))),
                },
                Err(e) => Err(anyhow!(format!("接收文件失败,{}", e))),
            }
        }
        InputCtrlCommand::Screen(save_path) => {
            //get data
            match context.wait_data(data_id).await {
                Ok(data) => {
                    let mut path = PathBuf::from(save_path.as_str());

                    if path.is_dir() {
                        path = path.join("1.png");
                    };
                    match file_util::save_file_with_unique_name(path.as_path(), &data).await {
                        Ok(p) => Ok(format!("保存Kik的截屏至:{}", p.to_string_lossy())),
                        Err(e) => Err(anyhow!(format!(
                            "保存Kik的截屏至:{}失败,error:{}",
                            save_path, e
                        ))),
                    }
                }
                Err(e) => Err(anyhow!(format!("接收文件失败,{}", e))),
            }
        }
        _ => {
            return Err(anyhow::Error::msg("不支持的类型"));
        }
    }?;
    Ok(ok_info)
}

async fn do_set_file(
    context: &Context,
    file_path: String,
    target_path: String,
) -> anyhow::Result<(CtrlCommand, CmdOptions, Option<PendingTransfer>)> {
    let data = file_util::read_file_limited(file_path, file_util::MAX_INLINE_FILE_BYTES).await?;
    let data_id = Uuid::new_v4().to_string();
    context
        .find_ctrl_data()
        .await
        .ok_or_else(|| anyhow!("应用数据传输通道未初始化"))?;
    let send_context = context.clone();
    let task_data_id = data_id.clone();
    let (start, start_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        start_rx
            .await
            .map_err(|_| anyhow!("文件发送任务未被放行"))?;
        send_context.send_data_with_id(&task_data_id, &data).await
    });
    Ok((
        CtrlCommand::SetFile(data_id, target_path),
        CmdOptions::default(),
        Some(PendingTransfer { start, task }),
    ))
}

async fn do_set_big_file(
    context: &Context,
    file_path: String,
    target_path: String,
) -> anyhow::Result<(CtrlCommand, CmdOptions, Option<PendingTransfer>)> {
    let cmd_options = CmdOptions::default().with_timeout(false);

    let data_id = Uuid::new_v4().to_string();
    let data_id_c = data_id.clone();
    context
        .find_ctrl_data()
        .await
        .ok_or_else(|| anyhow!("应用数据传输通道未初始化"))?;
    let prepared =
        file_util::prepare_big_file(&file_path, file_util::FILE_TRANSFER_CHUNK_BYTES).await?;
    let file_size = prepared.size;
    let hash = prepared.hash;
    let iter = prepared.stream;
    let send_context = context.clone();

    // 数据生产必须与控制请求并行，否则被控端会在命令处理中等待分片而形成死锁。
    // JoinHandle 返回给上层，服务端拒绝或连接中断时会被显式 abort，禁止任务脱离命令生命周期。
    let (start, start_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        start_rx
            .await
            .map_err(|_| anyhow!("大文件发送任务未被放行"))?;
        let result = send_big_file_parts(send_context.clone(), data_id_c.clone(), iter).await;
        match result {
            Ok(()) => Ok(()),
            Err((code, error)) => {
                let error_frame = Dok::Err(code).to_buf();
                let _ = send_context
                    .send_data_with_id(&data_id_c, &error_frame)
                    .await;
                Err(error)
            }
        }
    });
    // 发送hash
    Ok((
        CtrlCommand::SetBigFile(data_id, file_size, hash, target_path),
        cmd_options,
        Some(PendingTransfer { start, task }),
    ))
}

async fn receive_big_file(
    context: &Context,
    data_id: &str,
    save_path: &str,
    total: u64,
    expected_hash: &[u8],
) -> anyhow::Result<()> {
    let destination = PathBuf::from(save_path);
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).await?;
    let (temp_path, temp_file) = file_util::create_random_temp_file().await?;

    let result = match tokio::time::timeout(
        file_util::FILE_TRANSFER_TIMEOUT,
        receive_big_file_to_temp(
            context,
            data_id,
            total,
            expected_hash,
            temp_file,
            &temp_path,
            &destination,
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(anyhow!("大文件下载超过 4 小时总时限")),
    };
    if result.is_err() {
        let _ = fs::remove_file(&temp_path).await;
    }
    result
}

async fn receive_big_file_to_temp(
    context: &Context,
    data_id: &str,
    total: u64,
    expected_hash: &[u8],
    mut file: fs::File,
    temp_path: &Path,
    destination: &Path,
) -> anyhow::Result<()> {
    if expected_hash.len() != 32 {
        return Err(anyhow!("SHA-256 长度必须为 32 bytes"));
    }
    let mut tracker = file_util::FileRangeTracker::new(total)?;
    file.set_len(total).await?;
    let mut write_cursor = 0_u64;

    while !tracker.complete() {
        match Dok::from_buf(context.wait_data(data_id).await?)
            .ok_or_else(|| anyhow!("大文件分片格式错误"))?
        {
            Dok::FilePart(start, end, data) => {
                if tracker.register(start, end, data.len())?
                    == file_util::FileRangeRegistration::New
                {
                    file_util::write_range(&mut file, &mut write_cursor, start, end, &data).await?;
                }
            }
            Dok::Err(code) => return Err(anyhow!("发送端报告大文件传输失败: {code:?}")),
        }
    }

    file.flush().await?;
    file.sync_all().await?;
    let actual_hash = file_util::compute_open_file_hash(&mut file).await?;
    if actual_hash != expected_hash {
        return Err(anyhow!("大文件 SHA-256 校验失败"));
    }
    drop(file);
    file_util::commit_temp_file(temp_path, destination).await
}

/// 以固定窗口并发发送分片，让不同数据连接真正并行工作。
/// 首个失败出现后停止读取源文件，但会等待已开始的帧写完，避免取消写操作破坏 TCP 帧边界。
async fn send_big_file_parts(
    context: Context,
    data_id: String,
    mut stream: file_util::FileChunkStream,
) -> Result<(), (ErrCode, anyhow::Error)> {
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
                failure.get_or_insert((ErrCode::WriteError, anyhow!(error)));
            }
            None => break,
        }
    }

    match failure {
        Some(error) => Err(error),
        None if source_exhausted => Ok(()),
        None => Err((ErrCode::ReadError, anyhow!("文件分片流意外结束"))),
    }
}
