use bytes::BytesMut;
use common::channel::{Channel, ChannelType};
use common::config::{ClientTransportMode, Config};
use common::ltc_codec::{LengthFieldBasedFrameDecoder, CONTROL_MAX_FRAME_LENGTH};
use common::message::init_frame::InitFrame;
use common::protocol;
use common::protocol::BufSerializable;
use common::secure_transport::connect_real_ctrl;
use common::session_auth::{ctrl_auth_proof, random_nonce_hex};
use ctrl_common::ctrl_frame::Frame;
use ctrl_common::ctrl_protocol::ctrl_pong;
use ctrl_common::ctrl_resp::Resp::Server;
use ctrl_common::ctrl_resp::{CmdResp, ServerResp, ServerSuccessResp};
use log::debug;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::Mutex;
use tokio::time::{self, timeout};
use tokio_stream::StreamExt;
use tokio_util::codec::FramedRead;

const AUTH_OK_PREFIX: &str = "##authtrue:";

pub async fn ctrl_conn(
    config: &Config,
) -> anyhow::Result<(Arc<Mutex<Channel>>, Receiver<CmdResp>, Option<String>)> {
    let parts = connect_real_ctrl(config).await?;
    // 控制通道只承载命令和响应，使用较小帧上限避免异常输入占用过多内存。
    let framed_read = FramedRead::new(
        BufReader::new(parts.reader),
        LengthFieldBasedFrameDecoder::new_with_max_frame_len(CONTROL_MAX_FRAME_LENGTH),
    );
    let framed_arc = Arc::new(Mutex::new(framed_read));
    let channel_arc = Arc::new(Mutex::new(Channel::new(
        parts.writer,
        None,
        ChannelType::Unknown,
        parts.local_addr,
        parts.peer_addr,
    )));

    let channel = channel_arc.clone();
    let client_nonce = random_nonce_hex();
    let auth_secret = config.id.encrypt();
    let client_mode = config.security.client_mode.clone();
    e2e_trace("ctrl_conn: connected transport");
    handle_active(config, &client_nonce, channel.clone()).await?;
    e2e_trace("ctrl_conn: sent auth start");

    let (mut tx, mut rx) = mpsc::channel::<CmdResp>(5);
    tokio::spawn(async move {
        let client_nonce = client_nonce;
        let auth_secret = auth_secret;
        let chan = channel.clone();
        tokio::spawn(async move {
            heartbeat(chan).await;
        });

        loop {
            // 读锁只包住 next().await，避免后续处理逻辑需要同一 reader 时形成隐式自锁。
            let read_result = {
                let mut framed = framed_arc.lock().await;
                timeout(Duration::from_secs(45), framed.next()).await
            };

            match read_result {
                Ok(Some(Ok(msg))) => {
                    let channel = channel.clone();
                    if handle_read(
                        channel,
                        msg,
                        &mut tx,
                        &auth_secret,
                        &client_nonce,
                        &client_mode,
                    )
                    .await
                    .is_none()
                    {
                        debug!("控制连接读取处理失败");
                        break;
                    }
                }
                Ok(Some(Err(e))) => {
                    println!("连接异常:{}", e);
                    handle_error(channel.clone()).await;
                    break;
                }
                Ok(None) => {
                    break;
                }
                Err(_) => {
                    println!("超时未读，断开控制连接");
                    let _ = channel.clone().lock().await.write_half_close().await;
                    break;
                }
            };
        }

        let chan = channel.clone();
        tokio::spawn(async move { handle_inactive(chan).await });
    });

    // 第一次响应只用于控制通道鉴权确认，后续 rx 才承载业务响应。
    let session_id = match rx.recv().await {
        None => panic!("服务端未响应"),
        Some(res) => match res.get_resp() {
            Server(ServerResp::Success(ServerSuccessResp::Info(auth)))
                if auth.starts_with(AUTH_OK_PREFIX) =>
            {
                auth.strip_prefix(AUTH_OK_PREFIX)
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string)
            }
            _ => panic!("服务端返回了不支持的控制连接初始化响应"),
        },
    };

    channel_arc.lock().await.channel_type = ChannelType::Ctrl;
    debug!("控制连接校验成功");
    Ok((channel_arc, rx, session_id))
}

async fn heartbeat(channel: Arc<Mutex<Channel>>) {
    loop {
        time::sleep(Duration::from_secs(5)).await;
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
    config: &Config,
    client_nonce: &str,
    channel: Arc<Mutex<Channel>>,
) -> anyhow::Result<()> {
    let frame = match config.security.client_mode {
        ClientTransportMode::Plain => InitFrame::CtrlAuthReq(config.id.encrypt()),
        ClientTransportMode::PinnedTls => InitFrame::CtrlAuthStart(client_nonce.to_string()),
    };
    channel
        .lock()
        .await
        .write_and_flush(&protocol::transfer_encode_frame(frame))
        .await?;
    Ok(())
}

async fn handle_error(_channel: Arc<Mutex<Channel>>) {}

async fn handle_inactive(channel: Arc<Mutex<Channel>>) {
    let _ = channel.clone().lock().await.write_half_close().await;
}

async fn handle_read(
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
    tx: &mut Sender<CmdResp>,
    auth_secret: &str,
    client_nonce: &str,
    client_mode: &ClientTransportMode,
) -> Option<()> {
    let channel_type = channel.lock().await.channel_type.clone();
    if channel_type == ChannelType::Unknown {
        let frame = InitFrame::from_buf(msg)?;
        match frame {
            InitFrame::CtrlAuthChallenge(server_nonce) => {
                e2e_trace("ctrl_conn: received auth challenge");
                if *client_mode != ClientTransportMode::PinnedTls {
                    return None;
                }
                let proof = ctrl_auth_proof(auth_secret, client_nonce, &server_nonce);
                channel
                    .lock()
                    .await
                    .write_and_flush(&protocol::transfer_encode_frame(InitFrame::CtrlAuthProof {
                        client_nonce: client_nonce.to_string(),
                        proof,
                    }))
                    .await
                    .ok()?;
                e2e_trace("ctrl_conn: sent auth proof");
            }
            InitFrame::CtrlAuthSession(session_id) => {
                e2e_trace("ctrl_conn: received auth session");
                tx.send(CmdResp::new(
                    "##cmdId".to_string(),
                    Server(ServerResp::Success(ServerSuccessResp::Info(format!(
                        "{}{}",
                        AUTH_OK_PREFIX, session_id
                    )))),
                ))
                .await
                .ok()?;
            }
            InitFrame::CtrlAuthReply(true) => {
                // 初始化成功消息复用业务响应通道，只作为外层函数继续执行的信号。
                tx.send(CmdResp::new(
                    "##cmdId".to_string(),
                    Server(ServerResp::Success(ServerSuccessResp::Info(
                        AUTH_OK_PREFIX.to_string(),
                    ))),
                ))
                .await
                .ok()?;
            }
            InitFrame::CtrlAuthReply(false) => {
                debug!("控制连接业务鉴权失败");
                println!("账号或密码错误");
                std::process::exit(0);
            }
            _ => panic!("控制端收到不支持的初始化帧"),
        }
    } else {
        let frame = Frame::from_buf(msg)?;
        match frame {
            Frame::Resp(resp) => {
                tx.send(resp).await.ok()?;
            }
            Frame::Ping | Frame::Pong => {}
            f => {
                debug!("控制连接收到不支持的业务帧,{:?}", f);
                panic!("控制端不支持该帧")
            }
        };
    }

    Some(())
}

fn e2e_trace(message: &str) {
    let Some(path) = std::env::var("REAL_CTRL_E2E_TRACE_PATH")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return;
    };

    // 仅 E2E 测试显式开启，用于定位本地握手；生产默认不写任何 trace 文件。
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{message}");
    }
}
