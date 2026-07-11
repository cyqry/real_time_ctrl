use crate::core::context::Context;
use crate::handler::read_handle;
use anyhow::Error;
use bytes::BytesMut;
use common::channel::{Channel, ChannelType};
use common::config::Config;
use common::ltc_codec::{
    LengthFieldBasedFrameDecoder, CONTROL_MAX_FRAME_LENGTH, DATA_MAX_FRAME_LENGTH,
    INIT_MAX_FRAME_LENGTH,
};
use common::protocol::kik_ping;
use common::secure_transport::{
    accept_tls, build_server_tls_acceptor, split_stream, TransportParts,
};
use ctrl_common::ctrl_protocol::ctrl_ping;
use log::{debug, error, info, warn};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Semaphore};
use tokio::time::timeout;
use tokio::{io, time};
use tokio_stream::StreamExt;
use tokio_util::codec::FramedRead;

pub async fn run(context: Context, config: Config) -> anyhow::Result<()> {
    // 连接许可在握手前获取，避免 TLS/半帧慢连接无限创建任务并占满内存。
    let connection_limit = Arc::new(Semaphore::new(512));
    if let Some(tls_acceptor) = build_server_tls_acceptor(&config.security)? {
        let tls_listener = TcpListener::bind(format!(
            "{}:{}",
            config.server_host, config.security.tls_port
        ))
        .await?;
        info!(
            "开启 real_ctrl TLS 管理端口,监听{}的{}端口",
            config.server_host, config.security.tls_port
        );
        let tls_context = context.clone();
        let tls_config = config.clone();
        let tls_connection_limit = connection_limit.clone();
        tokio::spawn(async move {
            loop {
                match tls_listener.accept().await {
                    Ok((stream, addr)) => {
                        let Ok(permit) = tls_connection_limit.clone().try_acquire_owned() else {
                            warn!("活动连接达到上限，拒绝 TLS 连接: {}", addr);
                            continue;
                        };
                        let acceptor = tls_acceptor.clone();
                        let context = tls_context.clone();
                        let config = tls_config.clone();
                        tokio::spawn(async move {
                            let _permit = permit;
                            match accept_tls(acceptor, stream, config.read_timeout).await {
                                Ok(parts) => {
                                    handle_transport_parts(
                                        context,
                                        config,
                                        parts,
                                        addr,
                                        TransportPolicy::TlsControl,
                                    )
                                    .await;
                                }
                                Err(e) => {
                                    error!("TLS 握手失败，远程地址:{}，error:{}", addr, e);
                                }
                            }
                        });
                    }
                    Err(e) => {
                        error!("TLS 管理端口 accept 失败:{}", e);
                    }
                }
            }
        });
    } else {
        warn!("未配置 TLS 管理端口；明文端口默认只接收 ctrl_kik，real_ctrl 将无法接入");
    }

    let listener =
        TcpListener::bind(format!("{}:{}", config.server_host, config.server_port)).await?;
    info!(
        "开启服务,监听{}的{}端口",
        config.server_host, config.server_port
    );
    loop {
        let (stream, addr) = listener.accept().await?;
        let Ok(permit) = connection_limit.clone().try_acquire_owned() else {
            warn!("活动连接达到上限，拒绝明文连接: {}", addr);
            continue;
        };
        tokio::spawn(handle_stream(
            context.clone(),
            config.clone(),
            stream,
            addr,
            permit,
        ));
    }
}

#[derive(Clone, Copy)]
enum TransportPolicy {
    Plain,
    TlsControl,
}

async fn handle_stream(
    context: Context,
    config: Config,
    stream: TcpStream,
    local_addr: SocketAddr,
    _permit: tokio::sync::OwnedSemaphorePermit,
) {
    if let Err(e) = stream.set_nodelay(true) {
        debug!("设置 TCP_NODELAY 失败: {}", e);
    }
    let parts = split_stream(stream);
    handle_transport_parts(context, config, parts, local_addr, TransportPolicy::Plain).await;
}

async fn handle_transport_parts(
    context: Context,
    config: Config,
    parts: TransportParts,
    _remote_addr: SocketAddr,
    transport_policy: TransportPolicy,
) {
    // InitFrame 本身很小；只有角色认证完成后才允许数据通道切换到大帧上限。
    let framed_read = FramedRead::new(
        BufReader::new(parts.reader),
        LengthFieldBasedFrameDecoder::new_with_max_frame_len(INIT_MAX_FRAME_LENGTH),
    );
    let framed_arc = Arc::new(Mutex::new(framed_read));
    let channel_arc = Arc::new(Mutex::new(Channel::new(
        parts.writer,
        None,
        ChannelType::Unknown,
        parts.local_addr,
        parts.peer_addr,
    )));
    channel_arc
        .lock()
        .await
        .set_write_timeout(config.write_timeout);

    let channel = channel_arc.clone();

    let chan = channel.clone();
    tokio::spawn(async move {
        heartbeat(chan).await;
    });

    let e = loop {
        // 读锁必须在进入 match 前释放；否则后续调整 decoder 上限会再次锁同一个 FramedRead。
        let read_result = {
            let mut framed = framed_arc.lock().await;
            timeout(config.read_timeout, framed.next()).await
        };

        match read_result {
            Ok(res) => match res {
                Some(Ok(msg)) => {
                    let channel = channel.clone();
                    match handle_read(
                        config.clone(),
                        context.clone(),
                        channel.clone(),
                        msg,
                        transport_policy,
                    )
                    .await
                    {
                        Ok(_) => {
                            let channel_type = channel.lock().await.channel_type.clone();
                            let max_frame_len = max_frame_len_for_channel_type(&channel_type);
                            framed_arc
                                .lock()
                                .await
                                .decoder_mut()
                                .set_max_frame_len(max_frame_len);
                        }
                        Err(e) => {
                            break Some(e);
                        }
                    }
                    continue;
                }
                Some(Err(e)) => {
                    break Some(anyhow::Error::new(e));
                }

                None => {
                    break None;
                }
            },
            Err(e) => {
                let _ = channel.clone().lock().await.write_half_close().await;
                break Some(anyhow::Error::new(e));
            }
        };
    };
    let chan = channel.clone();
    if let Some(error) = e {
        handle_error(chan, error).await;
    }

    let chan = channel.clone();
    let context = context.clone();
    tokio::spawn(async move {
        handle_inactive(context, chan).await;
    });
}

async fn handle_error(chan: Arc<Mutex<Channel>>, error: Error) {
    let remote_addr = chan
        .lock()
        .await
        .get_peer_addr()
        .as_ref()
        .map(|addr| addr.to_string())
        .unwrap_or("未知远程地址".to_string());
    if error.is::<io::Error>() {
        println!("io错误，远程地址:{}，error:{}", remote_addr, error);
    } else if error.is::<time::error::Elapsed>() {
        println!("读取超时，远程地址:{}", remote_addr);
    } else {
        error!("处理连接错误，远程地址:{}，error:{}", remote_addr, error);
    }
}

async fn handle_inactive(context: Context, channel: Arc<Mutex<Channel>>) {
    //统一close
    channel.lock().await.try_write_half_close().await;

    let ip = channel
        .lock()
        .await
        .get_peer_addr()
        .as_ref()
        .map(|addr| addr.ip().to_string())
        .unwrap_or("未知ip".to_string());
    let channel_type = channel.lock().await.channel_type.clone();
    match channel_type {
        ChannelType::Ctrl => {
            context.delete_ctrl_conn_if(&channel).await;
        }
        ChannelType::CtrlData => {
            //清理
            context.delete_ctrl_data_conn(channel).await;
        }
        ChannelType::Kik => {
            // kik连接的id直接是kikid
            let id = channel.lock().await.get_id().to_string();
            // context.set_kik_state();
            // 因为Kik连接断开了，所以万一在被控制，需要清理
            let _ = context.delete_kik_conn_if_id(id.as_str()).await;
            if let Some(kik) = context.delete_kik_if_not_online(id.as_str()).await {
                info!("【{}】下线，ip:{}", kik.kik_client_info.kik_info.name, ip);
            }
        }
        ChannelType::KikData => {
            //清理
            context.delete_kik_data_conn(channel.clone()).await;
            let kik_id = channel.lock().await.get::<String>("kik_id").cloned();
            if let Some(kik_id) = kik_id {
                if let Some(kik) = context.delete_kik_if_not_online(&kik_id).await {
                    info!("【{}】下线，ip:{}", kik.kik_client_info.kik_info.name, ip);
                }
            }
        }
        ChannelType::Unknown => {
            //清理？？？？
        }
    };
}

async fn heartbeat(channel: Arc<Mutex<Channel>>) {
    loop {
        //延迟发ping
        time::sleep(Duration::from_secs(5)).await;
        if channel.lock().await.is_closed() {
            return;
        }
        let channel_type = channel.clone().lock().await.channel_type.clone();
        let ping = match channel_type {
            ChannelType::Ctrl | ChannelType::CtrlData => Some(ctrl_ping()),
            ChannelType::Kik | ChannelType::KikData => Some(kik_ping()),
            ChannelType::Unknown => None,
        };
        let Some(ping) = ping else { continue };

        //服务端要保证得到状态之后延迟一点发，因为要等对方接收确认
        time::sleep(Duration::from_secs(5)).await;
        match channel.lock().await.write_and_flush(&ping).await {
            Ok(_) => {}
            Err(_) => {
                break;
            }
        };
    }
}

fn max_frame_len_for_channel_type(channel_type: &ChannelType) -> usize {
    match channel_type {
        ChannelType::Ctrl | ChannelType::Kik => CONTROL_MAX_FRAME_LENGTH,
        ChannelType::CtrlData | ChannelType::KikData => DATA_MAX_FRAME_LENGTH,
        ChannelType::Unknown => INIT_MAX_FRAME_LENGTH,
    }
}

//由于初始化验证消息和业务消息是分开的，所以初始化过程中不能ping pong和发业务消息，所以服务端要么 确认 客户端接收到服务端确认 才能 ping pong， 要么 向客户端发送服务端确认之后 延迟发ping pong；这里采用后者方案
async fn handle_read(
    config: Config,
    context: Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
    transport_policy: TransportPolicy,
) -> anyhow::Result<()> {
    let channel_type = channel.clone().lock().await.channel_type.clone(); //这里不克隆直接match的话又会出现match的生命周期问题，导致死锁。
    match channel_type {
        ChannelType::Ctrl => {
            read_handle::handle_ctrl(context, channel, msg, config.security.allow_remote_exec).await
        }
        ChannelType::CtrlData => read_handle::handle_ctrl_data(context, channel, msg).await,
        ChannelType::Kik => read_handle::handle_kik(context, channel, msg).await,
        ChannelType::KikData => read_handle::handle_kik_data(context, channel, msg).await,
        ChannelType::Unknown => {
            //未识别的连接连ping pong 都不让发； unknow到其他消息状态的转换最好是同步的，不然有问题
            let allow_ctrl = matches!(transport_policy, TransportPolicy::TlsControl)
                || config.security.allow_plain_ctrl;
            let allow_kik = matches!(transport_policy, TransportPolicy::Plain);
            read_handle::handle_init_message(config, context, channel, msg, allow_ctrl, allow_kik)
                .await
        }
    }
}
