use crate::core::context::Context;
use bytes::BytesMut;
use common::channel::Channel;
use common::message::kik_frame::KikFrame;
use common::protocol::{self, BufSerializable};
use ctrl_common::ctrl_frame::Frame;
use log::info;
use std::sync::Arc;
use tokio::sync::Mutex;

fn default_error() -> anyhow::Error {
    anyhow::Error::msg("不支持的数据帧类型")
}
pub async fn handle_ctrl_data(
    context: Context,
    _: Arc<Mutex<Channel>>,
    msg: BytesMut,
) -> anyhow::Result<()> {
    let frame = Frame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    match frame {
        Frame::Data(id, data) => {
            // 在当前读循环中等待下游写入，利用 TCP 与有界连接队列形成背压；
            // 逐帧 spawn 会在慢客户端场景积累大块 BytesMut 并破坏文件分片顺序。
            if let Some(kik) = context.get_kik().await {
                if let Some(data_c) = kik.find_data_conn().await {
                    data_c
                        .lock()
                        .await
                        .write_and_flush(&protocol::transfer_encode_frame(KikFrame::Data(id, data)))
                        .await?;
                } else {
                    info!("当前kik没有数据连接")
                }
            } else {
                info!("数据发送失败当前没有在线kik")
            }
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
    context: Context,
    _channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
) -> anyhow::Result<()> {
    let frame = KikFrame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    match frame {
        KikFrame::Data(id, data) => {
            match context.find_ctrl_data().await {
                None => {
                    //未找到ctrl的data_conn或者根本没有ctrl,不管，
                }
                Some(c) => {
                    c.lock()
                        .await
                        .write_and_flush(&protocol::transfer_encode_frame(Frame::Data(id, data)))
                        .await?;
                }
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
