use crate::cmd_runner;
use crate::context::Context;
use bytes::BytesMut;
use common::channel::{Channel, ChannelType};
use common::command::{Command, SysCommand};
use common::config::Config;
use common::kik::Kik;
use common::message::frame::Frame;
use common::message::frame::Frame::{Cmd, CmdExtra, Resp};
use common::message::resp;
use common::message::resp::Resp::{DataId, Info};
use common::protocol;
use common::protocol::BufSerializable;
use log::debug;
use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{mpsc, Mutex};
use tokio::time;
use tokio::time::error::Elapsed;
use tokio::time::timeout;

fn default_error() -> anyhow::Error {
    anyhow::Error::msg("不支持的帧类型")
}

pub async fn handle_kik(
    context: &Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
    tx: &mut Sender<Box<dyn Any + Send + Sync>>,
) -> anyhow::Result<()> {
    let frame = Frame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    match frame {
        Frame::CmdExtra(cmd, cmd_id) => {
            debug!("开run,cmd_id:{}", cmd_id);
            let resp = match timeout(Duration::from_secs(60 * 5), cmd_runner::run(context, cmd)).await {
                Ok(resp) => {
                    resp
                }
                Err(_) => {
                    Info("Kik执行任务超时".to_string())
                }
            };

            debug!("开始响应:{:?},cmd_id:{}", resp, cmd_id);
            channel
                .clone()
                .lock()
                .await
                .write_and_flush(&protocol::transfer_encode(
                    Frame::RespExtra(resp, cmd_id).to_buf(),
                ))
                .await?;
            debug!("响应结束");
        }
        Frame::Ping => {}
        Frame::Pong => {}
        _ => {
            return Err(default_error());
        }
    }
    Ok(())
}

pub async fn handle_kik_data(
    context: &Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
    tx: &mut Sender<Box<dyn Any + Send + Sync>>,
) -> anyhow::Result<()> {
    let frame = Frame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    match frame {
        Frame::Data(id, data) => {
            context.send_data((id, data))
                .await
                .expect("不可能没有读者!");
        }
        Frame::Ping => {}
        Frame::Pong => {}
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
    let frame = Frame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    match frame {
        Frame::KikId(id) => {
            match tx.send(Box::new(id)).await {
                Ok(_) => {}
                Err(e) => {
                    //todo 写端关闭，神奇
                }
            };
        }
        Frame::Ping => {}
        Frame::Pong => {}
        f => {
            return Err(default_error());
        }
    }
    Ok(())
}
