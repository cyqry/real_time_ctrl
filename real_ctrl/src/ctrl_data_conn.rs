use crate::context::Context;
use anyhow::Error;
use bytes::BytesMut;
use common::channel::{Channel, ChannelType};
use common::config::{ClientTransportMode, Config};
use common::ltc_codec::{LengthFieldBasedFrameDecoder, DATA_MAX_FRAME_LENGTH};
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

pub async fn ctrl_data_conn(context: Context, config: &Config) -> anyhow::Result<()> {
    let parts = connect_real_ctrl(config).await?;
    // 数据通道当前兼容历史文件/截图传输，暂时使用较大的帧上限。
    let framed_read = FramedRead::new(
        BufReader::new(parts.reader),
        LengthFieldBasedFrameDecoder::new_with_max_frame_len(DATA_MAX_FRAME_LENGTH),
    );
    let framed_arc = Arc::new(Mutex::new(framed_read));
    let channel_arc = Arc::new(Mutex::new(Channel::new(
        parts.writer,
        Some(Uuid::new_v4().to_string()),
        ChannelType::Unknown,
        parts.local_addr,
        parts.peer_addr,
    )));

    let channel = channel_arc.clone();
    handle_active(&context, config, channel.clone()).await?;

    let (mut tx, mut rx) = mpsc::channel::<CmdResp>(5);
    let context_clone = context.clone();
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
                timeout(Duration::from_secs(45), framed.next()).await
            };

            match read_result {
                Ok(Some(Ok(msg))) => {
                    let channel = channel.clone();
                    if let Err(e) = handle_read(&context, channel, msg, &mut tx).await {
                        debug!("数据连接读取处理失败");
                        break Some(e);
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
    match rx.recv().await {
        None => panic!("服务端未响应"),
        Some(res) => match res.get_resp() {
            Server(ServerResp::Success(ServerSuccessResp::Info(auth))) if auth == "##authtrue" => {
                channel_arc.clone().lock().await.channel_type = ChannelType::CtrlData;
                context.insert_ctrl_data_conn(channel_arc).await;
                debug!("数据连接校验成功");
            }
            _ => panic!("服务端返回了不支持的数据连接初始化响应"),
        },
    };

    Ok(())
}

async fn handle_inactive(context: &Context, channel: Arc<Mutex<Channel>>) {
    channel.lock().await.try_write_half_close().await;
    let channel_type = channel.clone().lock().await.channel_type.clone();
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
                // v2 数据通道绑定成功，复用业务响应通道通知外层完成初始化。
                tx.send(CmdResp::new(
                    "##cmd_id".to_owned(),
                    Server(Success(ServerSuccessResp::Info("##authtrue".to_string()))),
                ))
                .await?;
            }
            InitFrame::CtrlDataSessionReply(false) => {
                debug!("数据控制连接 v2 会话绑定失败");
                println!("数据通道会话校验失败");
                std::process::exit(0);
            }
            InitFrame::CtrlDataConnAuthReply(true) => {
                // 初始化成功消息复用业务响应通道，只作为外层函数继续执行的信号。
                tx.send(CmdResp::new(
                    "##cmd_id".to_owned(),
                    Server(Success(ServerSuccessResp::Info("##authtrue".to_string()))),
                ))
                .await?;
            }
            InitFrame::CtrlDataConnAuthReply(false) => {
                debug!("数据控制连接业务鉴权失败");
                println!("账号或密码错误");
                std::process::exit(0);
            }
            f => {
                debug!("数据控制连接收到错误的初始化帧,{:?}", f);
                panic!("控制端不支持该帧")
            }
        }
    } else {
        let frame = Frame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
        match frame {
            Frame::Data(id, data) => {
                debug!("收到长度为{}的数据", data.len());
                context.get_data_tx().send((id, data)).await?;
            }
            Frame::Ping | Frame::Pong => {}
            f => {
                debug!("数据控制连接收到错误的业务帧,{:?}", f);
                panic!("控制端不支持该帧")
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
        if guard.channel_type != ChannelType::Unknown {
            if guard.write_and_flush(&ctrl_pong()).await.is_err() {
                break;
            }
        }
    }
}

async fn handle_active(
    context: &Context,
    config: &Config,
    channel: Arc<Mutex<Channel>>,
) -> anyhow::Result<()> {
    let frame = match config.security.client_mode {
        ClientTransportMode::Plain => InitFrame::CtrlDataConnReq(config.id.encrypt()),
        ClientTransportMode::PinnedTls => {
            let session_id = context
                .agent
                .read()
                .await
                .session_id
                .clone()
                .ok_or(anyhow::Error::msg("控制会话尚未建立，无法创建数据通道"))?;
            let channel_nonce = random_nonce_hex();
            let proof = ctrl_data_proof(&config.id.encrypt(), &session_id, &channel_nonce);
            InitFrame::CtrlDataSessionReq {
                session_id,
                channel_nonce,
                proof,
            }
        }
    };
    channel
        .lock()
        .await
        .write_and_flush(&protocol::transfer_encode_frame(frame))
        .await?;
    Ok(())
}
