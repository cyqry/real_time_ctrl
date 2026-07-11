use crate::cmd_runner;
use crate::context::Context;
use bytes::BytesMut;
use common::channel::Channel;
use common::command::Command;
use common::message::init_frame::InitFrame;
use common::message::kik_frame::KikFrame;
use common::message::kik_resp::kik_error;
use common::protocol;
use common::protocol::{BufSerializable, CmdOptions};
use log::debug;
use std::any::Any;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tokio::sync::Mutex;
use tokio::time::timeout;

fn default_error() -> anyhow::Error {
    anyhow::Error::msg("不支持的帧类型")
}

pub async fn handle_kik(
    _context: &Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
) -> anyhow::Result<()> {
    let frame = KikFrame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    match frame {
        //控制过程应由单独线程处理，不阻塞连接主线程,与ping pong分开
        KikFrame::Cmd(req_cmd) => {
            channel
                .lock()
                .await
                .get::<Sender<(String, CmdOptions, Command)>>("cmd_tx")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("命令处理队列未初始化"))?
                .send(req_cmd.split())
                .await
                .map_err(|_| anyhow::anyhow!("命令处理线程已关闭"))?;
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
    debug!("Running command: {:?}", cmd);
    let resp = {
        let runner = cmd_runner::run(&context, cmd);
        if cmd_options.timeout() {
            timeout(Duration::from_secs(60 * 5), runner)
                .await
                .unwrap_or_else(|_| kik_error("Kik执行任务超时".to_string()))
        } else {
            runner.await
        }
    };
    debug!("开始响应:{:?},cmd_id:{}", resp, cmd_id);
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
    }
    debug!("响应结束");
}

pub async fn handle_kik_data(
    context: &Context,
    _channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
) -> anyhow::Result<()> {
    let frame = KikFrame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    match frame {
        KikFrame::Data(id, data) => {
            debug!("得到长度为{}的数据", data.len());
            context.send_data((id, data)).await.unwrap_or_else(|e| {
                debug!("向接收通道发送数据失败,error:{}", e);
            });
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
    tx: &mut Sender<Box<dyn Any + Send + Sync>>,
) -> anyhow::Result<()> {
    //由于服务端延迟发ping 所以还未初始化完成的kik连接 一般不会收到服务器的 KikFrame::Ping
    let frame = InitFrame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    match frame {
        InitFrame::KikId(id) => {
            match tx.send(Box::new(id)).await {
                Ok(_) => {}
                Err(_error) => {
                    //todo 写端关闭，神奇
                }
            };
        }
        _frame => {
            return Err(default_error());
        }
    }
    Ok(())
}
