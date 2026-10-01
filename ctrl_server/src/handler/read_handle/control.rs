//! 已认证 Ctrl 主连接上的命令与响应编排。
//!
//! 读循环只做解析、策略检查和取得并发许可，然后为每条命令启动独立任务。系统命令在服务端完成；
//! 远程命令必须携带明确目标，改写关联 ID 后只等待该 Kik 的专属响应；无目标请求直接拒绝。

use crate::core::connection_meta::CTRL_SESSION_ID;
use crate::core::context::Context;
use bytes::BytesMut;
use common::channel::Channel;
use common::command::{Command, CtrlCommand, SysCommand};
use common::file_util::LONG_COMMAND_TIMEOUT;
use common::message::kik_frame::KikFrame;
use common::message::kik_resp::{ClientSuccessResp, KikResp};
use common::protocol::{self, BufSerializable, ReqCmd};
use ctrl_common::cmd_resp_info::{KikInfoVo, ServerCapabilities};
use ctrl_common::ctrl_frame::Frame;
use ctrl_common::ctrl_protocol::{ctrl_kik_resp, ctrl_server_resp_error, ctrl_server_resp_success};
use futures::stream;
use log::{debug, warn};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::timeout;
use tokio_stream::StreamExt;
use uuid::Uuid;

fn default_error() -> anyhow::Error {
    anyhow::Error::msg("不支持的帧类型")
}

/// 控制连接读循环只负责校验和分派；命令生命周期在独立任务内运行。
/// 这样慢文件、Exec 或慢 Kik 都不会阻塞同一连接上后续请求的读取。
pub async fn handle_ctrl(
    context: Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
    allow_remote_exec: bool,
) -> anyhow::Result<()> {
    match Frame::from_buf(msg).ok_or_else(default_error)? {
        Frame::TaskList(request) => {
            super::task_catalog::handle(context, channel, request, allow_remote_exec).await?;
        }
        frame @ (Frame::Cmd(_) | Frame::TargetedCmd(_, _) | Frame::Capabilities(_)) => {
            let session_id = channel
                .lock()
                .await
                .attribute(&CTRL_SESSION_ID)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("控制连接缺少会话绑定"))?;
            let (cmd_id, request) = match frame {
                Frame::Cmd(req) => (req.get_id().to_owned(), Some((req, None))),
                Frame::TargetedCmd(target, req) => {
                    (req.get_id().to_owned(), Some((req, Some(target))))
                }
                Frame::Capabilities(id) => (id, None),
                _ => unreachable!("上层 match 已限制帧类型"),
            };
            // 旧客户端的无目标远程命令不能回退到任何设备。保留请求 ID 返回迁移错误，
            // 让客户端能区分版本不匹配与设备离线，同时保证未登记路由、未触碰 Kik。
            if request.as_ref().is_some_and(|(req, target)| {
                !matches!(req.get_cmd(), Command::Sys(_)) && target.is_none()
            }) {
                write_error(
                    &channel,
                    cmd_id,
                    "远程命令必须携带 Kik ID，请升级客户端并使用 $local_use 选择设备".into(),
                )
                .await?;
                return Ok(());
            }
            if request.as_ref().is_some_and(|(req, _)| {
                matches!(req.get_cmd(), Command::Exec(_) | Command::RunTask(_))
            }) && !allow_remote_exec
            {
                write_error(
                    &channel,
                    cmd_id,
                    "服务端当前策略禁止 Exec，可通过 CTRL_SERVER_ALLOW_EXEC=1 覆盖".into(),
                )
                .await?;
                return Ok(());
            }

            let permits = match context.try_acquire_command(&session_id).await {
                Ok(permits) => permits,
                Err(error) => {
                    write_error(&channel, cmd_id, error.to_string()).await?;
                    return Ok(());
                }
            };
            tokio::spawn(async move {
                let Some((request, target)) = request else {
                    let _permits = permits;
                    // 能力查询也持有账号/会话许可，避免已认证客户端制造无界响应任务。
                    let capabilities = ServerCapabilities {
                        target_bound_command_v1: true,
                        task_run_v1: true,
                        task_list_v1: true,
                    };
                    if let Ok(json) = serde_json::to_string(&capabilities) {
                        let _ = channel
                            .lock()
                            .await
                            .write_and_flush(&ctrl_server_resp_success(cmd_id, json))
                            .await;
                    }
                    return;
                };
                let (_, cmd_options, cmd) = request.split();
                if let Command::RunTask(name) = cmd {
                    let result = super::task_run::execute(
                        &context,
                        &channel,
                        &session_id,
                        &cmd_id,
                        &name,
                        target.as_deref(),
                        permits,
                    )
                    .await;
                    if let Err(error) = result {
                        let _ = write_error(&channel, cmd_id, error.to_string()).await;
                    }
                    return;
                }
                let _permits = permits;
                // session ID 属于数据通道绑定材料；日志只保留服务端随机命令 ID。
                debug!("处理控制命令: cmd_id={}", cmd_id);
                if let Err(error) = execute_command(
                    context,
                    channel.clone(),
                    session_id,
                    cmd_id.clone(),
                    cmd_options,
                    cmd,
                    target,
                )
                .await
                {
                    warn!("控制命令执行失败: cmd_id={}, error={}", cmd_id, error);
                    let _ = write_error(&channel, cmd_id, "命令执行失败".to_string()).await;
                }
            });
        }
        Frame::DataAck(data_id) => {
            let session_id = channel
                .lock()
                .await
                .attribute(&CTRL_SESSION_ID)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("控制连接缺少会话绑定"))?;
            context.finish_download_route(&session_id, &data_id).await;
        }
        Frame::Ping | Frame::Pong => {}
        _ => return Err(default_error()),
    }
    Ok(())
}

async fn execute_command(
    context: Context,
    channel: Arc<Mutex<Channel>>,
    session_id: String,
    cmd_id: String,
    cmd_options: common::protocol::CmdOptions,
    cmd: Command,
    target: Option<String>,
) -> anyhow::Result<()> {
    match cmd {
        Command::Sys(sys) => execute_system(&context, &channel, &session_id, cmd_id, sys).await,
        remote => {
            execute_remote(
                &context,
                &channel,
                &session_id,
                cmd_id,
                cmd_options,
                remote,
                target.as_deref(),
            )
            .await
        }
    }
}

async fn execute_system(
    context: &Context,
    channel: &Arc<Mutex<Channel>>,
    session_id: &str,
    cmd_id: String,
    command: SysCommand,
) -> anyhow::Result<()> {
    let encoded = match command {
        SysCommand::List => {
            let kiks = context.get_can_ctrl_kik(session_id).await?;
            let list: Vec<KikInfoVo> = stream::iter(kiks)
                .then(|(id, kik)| async move {
                    KikInfoVo {
                        id,
                        name: kik.kik_client_info.kik_info.name,
                        ip: kik.kik_client_info.ip.read().await.to_string(),
                        recent_online_time: *kik.kik_client_info.recent_online_time.read().await,
                    }
                })
                .collect()
                .await;
            ctrl_server_resp_success(cmd_id, serde_json::to_string(&list)?)
        }
        SysCommand::History(kik_id) => {
            let records = context
                .kik_presence_for_session(session_id, kik_id.as_deref())
                .await?;
            if kik_id.is_some() && records.is_empty() {
                ctrl_server_resp_error(cmd_id, "找不到该 Kik 的上下线记录".into())
            } else {
                ctrl_server_resp_success(cmd_id, serde_json::to_string(&records)?)
            }
        }
    };
    channel.lock().await.write_and_flush(&encoded).await
}

async fn execute_remote(
    context: &Context,
    channel: &Arc<Mutex<Channel>>,
    session_id: &str,
    external_cmd_id: String,
    cmd_options: common::protocol::CmdOptions,
    command: Command,
    target: Option<&str>,
) -> anyhow::Result<()> {
    let Some(target) = target else {
        return write_error(channel, external_cmd_id, "远程命令缺少目标 Kik ID".into()).await;
    };
    let kik = match context.get_authorized_target(session_id, target).await {
        Ok(kik) => kik,
        Err(error) => return write_error(channel, external_cmd_id, error.to_string()).await,
    };
    let Some(kik_conn) = kik.get_kik_conn().await else {
        return write_error(channel, external_cmd_id, "被控制的Kik已下线".into()).await;
    };
    let kik_id = kik
        .id()
        .ok_or_else(|| anyhow::anyhow!("已注册 Kik 缺少实例 ID"))?
        .to_string();
    let _kik_permit = match kik.try_acquire_command() {
        Ok(permit) => permit,
        Err(_) => {
            return write_error(channel, external_cmd_id, "该 Kik 的命令并发达到上限".into()).await;
        }
    };

    let upload_external_id = upload_data_id(&command).map(ToString::to_string);
    let internal_cmd_id = Uuid::new_v4().to_string();
    let response_rx = kik.register_command(internal_cmd_id.clone()).await?;
    let routed_command = match context
        .prepare_data_route(session_id, &kik_id, command)
        .await
    {
        Ok(command) => command,
        Err(error) => {
            kik.cancel_command(&internal_cmd_id).await;
            return write_error(channel, external_cmd_id, error.to_string()).await;
        }
    };
    let expected_download_id = download_data_id(&routed_command).map(ToString::to_string);
    let request = protocol::transfer_encode_frame(KikFrame::Cmd(ReqCmd::new(
        internal_cmd_id.clone(),
        cmd_options.clone(),
        routed_command,
    )));
    if let Err(error) = kik_conn.lock().await.write_and_flush(&request).await {
        kik.cancel_command(&internal_cmd_id).await;
        context
            .finish_upload_route(session_id, upload_external_id.as_deref())
            .await;
        finish_failed_download_route(context, session_id, expected_download_id.as_deref()).await;
        warn!("向被控端写入控制命令失败: {error}");
        return write_error(channel, external_cmd_id, "向被控端发送命令失败".into()).await;
    }

    let response_timeout = if cmd_options.timeout() {
        Duration::from_secs(60 * 5)
    } else {
        LONG_COMMAND_TIMEOUT
    };
    let response = match timeout(response_timeout, response_rx).await {
        Ok(Ok(response)) => response,
        Ok(Err(_)) => KikResp::Error(1, "被控端下线".to_string()),
        Err(_) => {
            kik.cancel_command(&internal_cmd_id).await;
            KikResp::Error(1, "被控端执行命令超时".to_string())
        }
    };
    context
        .finish_upload_route(session_id, upload_external_id.as_deref())
        .await;

    let download_state = classify_download_response(&response, expected_download_id.as_deref());
    if download_state != DownloadResponseState::RouteActive {
        finish_failed_download_route(context, session_id, expected_download_id.as_deref()).await;
    }
    let response = if download_state == DownloadResponseState::MismatchedId {
        KikResp::Error(1, "被控端返回了不匹配的数据关联 ID".to_string())
    } else {
        response
    };
    let write_result = channel
        .lock()
        .await
        .write_and_flush(&ctrl_kik_resp(external_cmd_id, response))
        .await;
    if write_result.is_err() && download_state == DownloadResponseState::RouteActive {
        finish_failed_download_route(context, session_id, expected_download_id.as_deref()).await;
    }
    write_result
}

fn upload_data_id(command: &Command) -> Option<&str> {
    match command {
        Command::Ctrl(CtrlCommand::SetFile(id, _))
        | Command::Ctrl(CtrlCommand::SetBigFile(id, _, _, _)) => Some(id),
        _ => None,
    }
}

fn download_data_id(command: &Command) -> Option<&str> {
    match command {
        Command::Ctrl(CtrlCommand::GetFile(_, id))
        | Command::Ctrl(CtrlCommand::GetBigFile(_, id))
        | Command::Ctrl(CtrlCommand::Screen(id)) => Some(id),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DownloadResponseState {
    NotDownload,
    RouteActive,
    RemoteError,
    MismatchedId,
}

/// 下载成功响应必须回显服务端生成的数据 ID；业务错误本身没有数据 ID，
/// 应原样返回并释放路由，不能伪装成关联 ID 不匹配。
fn classify_download_response(
    response: &KikResp,
    expected_id: Option<&str>,
) -> DownloadResponseState {
    let Some(expected_id) = expected_id else {
        return DownloadResponseState::NotDownload;
    };
    match response {
        KikResp::Success(ClientSuccessResp::DataId(id))
        | KikResp::Success(ClientSuccessResp::BigFile { data_id: id, .. })
            if id == expected_id =>
        {
            DownloadResponseState::RouteActive
        }
        KikResp::Error(_, _) => DownloadResponseState::RemoteError,
        KikResp::Success(_) => DownloadResponseState::MismatchedId,
    }
}

async fn finish_failed_download_route(
    context: &Context,
    session_id: &str,
    expected_id: Option<&str>,
) {
    if let Some(data_id) = expected_id {
        context.finish_download_route(session_id, data_id).await;
    }
}

async fn write_error(
    channel: &Arc<Mutex<Channel>>,
    cmd_id: String,
    message: String,
) -> anyhow::Result<()> {
    channel
        .lock()
        .await
        .write_and_flush(&ctrl_server_resp_error(cmd_id, message))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::channel::ChannelType;
    use ctrl_common::{
        ctrl_resp::{Resp, ServerResp},
        kik::Kik,
    };
    use std::{io, time::SystemTime};
    use tokio::io::AsyncReadExt;

    /// 即使恰好只有一台在线设备，无目标命令也不能被“好心”路由过去。
    /// 这里调用真实认证后分派入口，并检查被控连接没有收到任何业务字节。
    #[tokio::test]
    async fn untargeted_remote_commands_are_rejected_without_touching_any_kik() {
        let context = Context::init();
        let (mut control_peer, writer) = tokio::io::duplex(8192);
        let channel = Arc::new(Mutex::new(Channel::new(
            Box::pin(writer),
            Some("controller".into()),
            ChannelType::Ctrl,
            Err(io::Error::from(io::ErrorKind::NotConnected)),
            Err(io::Error::from(io::ErrorKind::NotConnected)),
        )));
        context
            .register_ctrl_session(
                channel.clone(),
                "session".into(),
                "default".into(),
                "instance".into(),
            )
            .await
            .unwrap();
        let (mut kik_peer, writer) = tokio::io::duplex(8192);
        let kik_channel = Arc::new(Mutex::new(Channel::new(
            Box::pin(writer),
            Some("device-a".into()),
            ChannelType::Kik,
            Err(io::Error::from(io::ErrorKind::NotConnected)),
            Err(io::Error::from(io::ErrorKind::NotConnected)),
        )));
        let kik = Kik::new(
            "device-a",
            "test",
            "127.0.0.1".into(),
            SystemTime::now(),
            kik_channel,
        );
        kik.set_kik_initialized(true);
        context.kiks.write().await.insert("device-a".into(), kik);
        for (index, command) in [
            Command::Exec("echo bounded-test".into()),
            Command::Ctrl(CtrlCommand::Ls(".".into())),
            Command::RunTask("task_a".into()),
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("missing-target-{index}");
            let request = ReqCmd::new(id.clone(), common::protocol::CmdOptions::default(), command);
            handle_ctrl(
                context.clone(),
                channel.clone(),
                Frame::Cmd(request).to_buf(),
                true,
            )
            .await
            .unwrap();
            let length = control_peer.read_u32().await.unwrap();
            let mut body = vec![0; length as usize];
            control_peer.read_exact(&mut body).await.unwrap();
            let Some(Frame::Resp(response)) = Frame::from_buf(BytesMut::from(body.as_slice()))
            else {
                panic!("expected correlated response")
            };
            assert_eq!(response.get_cmd_id(), &id);
            assert!(
                matches!(response.get_resp(), Resp::Server(ServerResp::Error(_, message)) if message.contains("Kik ID"))
            );
        }
        let mut byte = [0];
        assert!(timeout(Duration::from_millis(30), kik_peer.read(&mut byte))
            .await
            .is_err());
        // 版本不兼容的请求只失败自身，不能破坏认证会话或合法列表查询。
        assert_eq!(context.get_can_ctrl_kik("session").await.unwrap().len(), 1);
        assert!(context.session_auth_secret("session").await.is_some());
    }

    #[test]
    fn download_business_error_is_not_reported_as_id_mismatch() {
        let response = KikResp::Error(7, "remote file read failed".to_string());
        assert_eq!(
            classify_download_response(&response, Some("expected-data-id")),
            DownloadResponseState::RemoteError
        );
        match response {
            KikResp::Error(code, message) => {
                assert_eq!(code, 7);
                assert_eq!(message, "remote file read failed");
            }
            KikResp::Success(_) => panic!("测试响应必须保持为业务错误"),
        }
    }

    #[test]
    fn download_success_requires_exact_data_id() {
        let matching = KikResp::Success(ClientSuccessResp::DataId("expected".to_string()));
        let mismatching = KikResp::Success(ClientSuccessResp::DataId("unexpected".to_string()));
        let wrong_kind = KikResp::Success(ClientSuccessResp::Info("unexpected".to_string()));

        assert_eq!(
            classify_download_response(&matching, Some("expected")),
            DownloadResponseState::RouteActive
        );
        assert_eq!(
            classify_download_response(&mismatching, Some("expected")),
            DownloadResponseState::MismatchedId
        );
        assert_eq!(
            classify_download_response(&wrong_kind, Some("expected")),
            DownloadResponseState::MismatchedId
        );
    }
}
