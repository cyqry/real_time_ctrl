//! CtrlData 连接的建立、会话绑定和数据读循环。
//!
//! 每条数据连接重新完成 pinned TLS，再用主连接取得的 session 和一次性 nonce 做 HMAC 绑定。认证成功后
//! 才加入 `Context` 连接池并切换数据帧上限；断线只移除本连接，不影响同会话其他数据连接。

use crate::context::Context;
use crate::ctrl_conn::{frame_kind, init_frame_kind};
use anyhow::Error;
use bytes::BytesMut;
use common::channel::{Channel, ChannelType};
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
use ctrl_common::ctrl_resp::Resp::Server;
use ctrl_common::ctrl_resp::ServerResp::Success;
use ctrl_common::ctrl_resp::{CmdResp, ServerResp, ServerSuccessResp};
use log::debug;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::sync::mpsc::{self, Sender};
use tokio::sync::Mutex;
use tokio::time::{self, timeout};
use tokio_stream::StreamExt;
use tokio_util::codec::FramedRead;
use uuid::Uuid;

/// 建立一条 CtrlData 连接，等待服务端确认绑定后加入共享连接池。
pub async fn ctrl_data_conn(context: Context, config: &Config) -> anyhow::Result<()> {
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

    let channel = channel_arc.clone();
    handle_active(&context, config, channel.clone()).await?;

    let (mut tx, mut rx) = mpsc::channel::<CmdResp>(5);
    let context_clone = context.clone();
    let read_timeout = config.read_timeout;
    tokio::spawn(async move {
        let context = context_clone;

        let chan = channel.clone();
        tokio::spawn(async move {
            heartbeat(chan).await;
        });

        let e = loop {
            // 读锁只包住 next().await，避免 match 臂内处理逻辑被临时锁生命周期拖住。
            let read_result = {
                let mut framed = framed_arc.lock().await;
                timeout(read_timeout, framed.next()).await
            };

            match read_result {
                Ok(Some(Ok(msg))) => {
                    let channel = channel.clone();
                    if let Err(e) = handle_read(&context, channel.clone(), msg, &mut tx).await {
                        debug!("数据连接读取处理失败");
                        break Some(e);
                    }
                    if channel.lock().await.channel_type == ChannelType::CtrlData {
                        framed_arc
                            .lock()
                            .await
                            .decoder_mut()
                            .set_max_frame_len(DATA_MAX_FRAME_LENGTH);
                    }
                }
                Ok(Some(Err(e))) => {
                    println!("连接异常:{}", e);
                    break Some(anyhow::Error::new(e));
                }
                Ok(None) => break None,
                Err(e) => {
                    println!("超时未读，断开数据连接");
                    let _ = channel.clone().lock().await.write_half_close().await;
                    break Some(anyhow::Error::new(e));
                }
            };
        };

        let chan = channel.clone();
        if let Some(e) = e {
            handle_error(chan, e).await;
        }

        let chan = channel.clone();
        let context = context.clone();
        tokio::spawn(async move { handle_inactive(&context, chan).await });
    });

    // 第一次响应只用于数据通道鉴权确认，后续 rx 才承载异常路径信号。
    let auth_response = timeout(config.read_timeout, rx.recv())
        .await
        .map_err(|_| anyhow::anyhow!("等待数据通道鉴权响应超时"))?
        .ok_or_else(|| anyhow::anyhow!("数据连接在鉴权完成前断开"))?;
    match auth_response.get_resp() {
        Server(ServerResp::Success(ServerSuccessResp::Info(auth))) if auth == "##authtrue" => {
            context.insert_ctrl_data_conn(channel_arc).await?;
            debug!("数据连接校验成功");
        }
        _ => return Err(anyhow::anyhow!("服务端返回了不支持的数据连接初始化响应")),
    };

    Ok(())
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

async fn handle_read(
    context: &Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
    tx: &mut Sender<CmdResp>,
) -> anyhow::Result<()> {
    if channel.lock().await.channel_type == ChannelType::Unknown {
        let init_frame = InitFrame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
        match init_frame {
            InitFrame::CtrlDataSessionReply(true) => {
                // 数据通道绑定成功，复用业务响应通道通知外层完成初始化。
                channel.lock().await.channel_type = ChannelType::CtrlData;
                tx.send(CmdResp::new(
                    "##cmd_id".to_owned(),
                    Server(Success(ServerSuccessResp::Info("##authtrue".to_string()))),
                ))
                .await?;
            }
            InitFrame::CtrlDataSessionReply(false) => {
                debug!("数据控制连接会话绑定失败");
                return Err(anyhow::anyhow!("数据通道会话校验失败"));
            }
            f => {
                debug!(
                    "数据控制连接收到错误的初始化帧: kind={}",
                    init_frame_kind(&f)
                );
                return Err(anyhow::anyhow!("控制端不支持该初始化帧"));
            }
        }
    } else {
        let frame = Frame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
        match frame {
            Frame::Data(id, data) => {
                debug!("收到长度为{}的数据", data.len());
                // 有界队列等待超时后主动断开数据连接，避免消费者异常时永久占住读循环。
                context.enqueue_data((id, data)).await?;
            }
            Frame::Ping | Frame::Pong => {}
            f => {
                debug!("数据控制连接收到错误的业务帧: kind={}", frame_kind(&f));
                return Err(anyhow::anyhow!("控制端不支持该数据帧"));
            }
        };
    }

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
            break;
        }
    }
}

async fn handle_active(
    context: &Context,
    config: &Config,
    channel: Arc<Mutex<Channel>>,
) -> anyhow::Result<()> {
    let session_id = context
        .agent
        .read()
        .await
        .session_id
        .clone()
        .ok_or(anyhow::Error::msg("控制会话尚未建立，无法创建数据通道"))?;
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
