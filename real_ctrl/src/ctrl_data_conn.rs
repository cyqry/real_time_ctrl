//! CtrlData 连接的建立、会话绑定和数据读循环。
//!
//! 每条数据连接重新完成 pinned TLS，再用主连接取得的 session 和一次性 nonce 做 HMAC 绑定。认证成功后
//! 才加入 `Context` 连接池并切换数据帧上限；断线只移除本连接，不影响同会话其他数据连接。

use crate::context::Context;
use crate::ctrl_conn::{frame_kind, init_frame_kind};
use anyhow::Error;
use bytes::BytesMut;
use common::channel::{Channel, ChannelType, DATA_CHANNEL_IO_TIMEOUT};
use common::config::Config;
use common::ltc_codec::{
    LengthFieldBasedFrameDecoder, DATA_MAX_FRAME_LENGTH, INIT_MAX_FRAME_LENGTH,
};
use common::message::init_frame::InitFrame;
use common::protocol;
use common::protocol::BufSerializable;
use common::secure_transport::connect_real_ctrl;
use common::session_auth::{ctrl_data_proof, random_nonce_hex};
use ctrl_common::ctrl_frame::Frame;
use ctrl_common::ctrl_protocol::ctrl_pong;
use log::debug;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::{self, timeout};
use tokio_stream::StreamExt;
use tokio_util::codec::FramedRead;
use uuid::Uuid;

/// 建立一条 CtrlData 连接，等待服务端确认绑定后加入共享连接池。
pub async fn ctrl_data_conn(
    context: Context,
    config: &Config,
    expected_session_id: &str,
) -> anyhow::Result<JoinHandle<()>> {
    let parts = connect_real_ctrl(config).await?;
    // 数据通道承载文件和截图，鉴权完成后切换到数据帧上限。
    let framed_read = FramedRead::new(
        BufReader::new(parts.reader),
        LengthFieldBasedFrameDecoder::new_with_max_frame_len(INIT_MAX_FRAME_LENGTH),
    );
    let framed_arc = Arc::new(Mutex::new(framed_read));
    let channel_arc = Arc::new(Mutex::new(Channel::new(
        parts.writer,
        Some(Uuid::new_v4().to_string()),
        ChannelType::Unknown,
        parts.local_addr,
        parts.peer_addr,
    )));
    channel_arc
        .lock()
        .await
        .set_write_timeout(config.write_timeout);

    // 认证阶段不派生后台任务。若主 session 在 TLS/HMAC 握手期间换代，取消本 future 就会直接
    // 释放 reader/writer，不会留下无人管理的半初始化连接。
    handle_active(config, expected_session_id, channel_arc.clone()).await?;
    let init_message = {
        let mut framed = framed_arc.lock().await;
        timeout(config.read_timeout, framed.next())
            .await
            .map_err(|_| anyhow::anyhow!("等待数据通道鉴权响应超时"))?
            .ok_or_else(|| anyhow::anyhow!("数据连接在鉴权完成前断开"))??
    };
    match InitFrame::from_buf(init_message) {
        Some(InitFrame::CtrlDataSessionReply(true)) => {}
        Some(InitFrame::CtrlDataSessionReply(false)) => {
            return Err(anyhow::anyhow!("数据通道会话校验失败"));
        }
        Some(frame) => {
            debug!(
                "数据控制连接收到错误的初始化帧: kind={}",
                init_frame_kind(&frame)
            );
            return Err(anyhow::anyhow!("控制端不支持该数据初始化帧"));
        }
        None => return Err(anyhow::anyhow!("数据初始化帧格式错误")),
    }
    {
        let mut channel = channel_arc.lock().await;
        channel.channel_type = ChannelType::CtrlData;
        channel.set_write_timeout(DATA_CHANNEL_IO_TIMEOUT);
    }
    framed_arc
        .lock()
        .await
        .decoder_mut()
        .set_max_frame_len(DATA_MAX_FRAME_LENGTH);
    context
        .insert_ctrl_data_conn(expected_session_id, channel_arc.clone())
        .await?;
    debug!("数据连接校验成功");

    let context_clone = context.clone();
    let channel = channel_arc.clone();
    let handle = tokio::spawn(async move {
        let heartbeat_task = tokio::spawn(heartbeat(channel.clone()));
        let error = loop {
            let read_result = {
                let mut framed = framed_arc.lock().await;
                timeout(DATA_CHANNEL_IO_TIMEOUT, framed.next()).await
            };
            match read_result {
                Ok(Some(Ok(msg))) => match handle_data_frame(&context_clone, msg).await {
                    Ok(()) => {}
                    Err(error) => break Some(error),
                },
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
        // 监督器只有在本连接完成池删除后才会补建，避免旧、新连接短暂同时占用服务端配额。
        handle_inactive(&context_clone, channel).await;
    });
    Ok(handle)
}

async fn handle_inactive(context: &Context, channel: Arc<Mutex<Channel>>) {
    channel.lock().await.try_write_half_close().await;
    let channel_type = channel.lock().await.channel_type;
    if channel_type == ChannelType::CtrlData {
        context.delete_ctrl_data_conn(channel).await;
    }
}

async fn handle_error(_channel: Arc<Mutex<Channel>>, error: Error) {
    println!("{}", error);
}

async fn handle_data_frame(context: &Context, msg: BytesMut) -> anyhow::Result<()> {
    let frame = Frame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    match frame {
        Frame::Data(id, data) => {
            debug!("收到长度为{}的数据", data.len());
            // 有界队列等待超时后主动断开数据连接，避免消费者异常时永久占住读循环。
            context.enqueue_data((id, data)).await?;
        }
        Frame::Ping | Frame::Pong => {}
        frame => {
            debug!("数据控制连接收到错误的业务帧: kind={}", frame_kind(&frame));
            return Err(anyhow::anyhow!("控制端不支持该数据帧"));
        }
    };
    Ok(())
}

async fn heartbeat(channel: Arc<Mutex<Channel>>) {
    loop {
        time::sleep(Duration::from_secs(5)).await;
        if channel.lock().await.is_closed() {
            return;
        }
        let arc = channel.clone();
        let mut guard = arc.lock().await;
        if guard.channel_type != ChannelType::Unknown
            && guard.write_and_flush(&ctrl_pong()).await.is_err()
        {
            // 写失败后主动发送 FIN，使服务端和本地读循环尽快完成清理，缩短连接池缺口。
            guard.try_write_half_close().await;
            break;
        }
    }
}

async fn handle_active(
    config: &Config,
    expected_session_id: &str,
    channel: Arc<Mutex<Channel>>,
) -> anyhow::Result<()> {
    // 监督槽位固定绑定创建它时的 session。主连接换代后，旧握手即使迟到成功也不能混入新连接池。
    let session_id = expected_session_id.to_string();
    let channel_nonce = random_nonce_hex();
    let proof = ctrl_data_proof(
        config.id.control_plane_secret(),
        &session_id,
        &channel_nonce,
    );
    let frame = InitFrame::CtrlDataSessionReq {
        session_id,
        channel_nonce,
        proof,
    };
    channel
        .lock()
        .await
        .write_and_flush(&protocol::transfer_encode_frame(frame))
        .await?;
    Ok(())
}
