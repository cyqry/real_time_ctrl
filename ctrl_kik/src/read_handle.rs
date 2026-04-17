use crate::cmd_runner;
use crate::context::Context;
use bytes::BytesMut;
use common::channel::{Channel, ChannelType};
use common::command::{Command, SysCommand};
use common::config::Config;
use common::kik::Kik;
use common::message::kik_frame::{ KikFrame};
use common::message::kik_resp;
use common::protocol;
use common::protocol::{BufSerializable, CmdOptions};
use log::debug;
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::{Receiver, Sender, UnboundedReceiver, UnboundedSender};
use tokio::sync::{mpsc, Mutex};
use tokio::time;
use tokio::time::error::Elapsed;
use tokio::time::timeout;
use common::message::init_frame::InitFrame;
use common::message::kik_resp::kik_error;

fn default_error() -> anyhow::Error {
    anyhow::Error::msg("不支持的帧类型")
}

pub async fn handle_kik(
    context: &Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut
) -> anyhow::Result<()> {
    let frame = KikFrame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    match frame {
        //控制过程应由单独线程处理，不阻塞连接主线程,与ping pong分开
        KikFrame::Cmd(req_cmd) => {
            channel
                .lock()
                .await
                .get::<UnboundedSender<(String, CmdOptions, Command)>>("cmd_tx")
                .expect("没有命令发送者")
                .send(req_cmd.split())
                .expect("处理线程关闭");
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
            timeout(Duration::from_secs(60 * 5), runner).await.unwrap_or_else(|_| kik_error("Kik执行任务超时".to_string()))
        } else {
            runner.await
        }
    };

    debug!("开始响应:{:?},cmd_id:{}", resp, cmd_id);
    let suc = channel
        .lock()
        .await
        .write_and_flush(&protocol::transfer_encode_frame(
            KikFrame::RespExtra(resp, cmd_id),
        ))
        .await;
    //当发送失败
    if suc.is_err() {
        channel.lock().await.try_write_half_close().await;
    }
    debug!("响应结束");
}

pub async fn handle_kik_data(
    context: &Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut
) -> anyhow::Result<()> {

    let frame = KikFrame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    match frame {
        KikFrame::Data(id, data) => {
            // debug!("kik收到长度{}的数据",data.len());
            context.send_data((id, data)).await.unwrap_or(());
            // debug!("向接收通道发送数据失败,err:{}",e);
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
    context: &Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
    tx: &mut Sender<Box<dyn Any + Send + Sync>>,
) -> anyhow::Result<()> {

    //由于服务端延迟发ping 所以还未初始化完成的kik连接 一般不会收到服务器的 KikFrame::Ping
    let frame = InitFrame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    match frame {
        InitFrame::KikId(id) => {
            match tx.send(Box::new(id)).await {
                Ok(_) => {}
                Err(e) => {
                    //todo 写端关闭，神奇
                }
            };
        }
        f => {
            return Err(default_error());
        }
    }
    Ok(())
}
