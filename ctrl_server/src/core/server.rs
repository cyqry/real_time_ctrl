use crate::core::connection_meta::KIK_ID;
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
use common::noise_transport::{accept_kik_noise, build_kik_noise_acceptor};
use common::protocol::kik_ping;
use common::secure_transport::{
    accept_prefixed_tls, accept_tls, build_server_tls_acceptor, ServerTlsAcceptor, TransportParts,
    CTRL_TLS_PREFIX,
};
use ctrl_common::ctrl_protocol::ctrl_ping;
use log::{error, info, warn};
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
    let tls_acceptor = build_server_tls_acceptor(&config.security)?;
    let kik_noise_acceptor =
        build_kik_noise_acceptor(config.security.kik_noise_private_key.as_deref())?;
    if config.security.tls_port == config.server_port {
        return Err(anyhow::anyhow!(
            "Kik Noise 与 real_ctrl TLS 必须使用不同端口"
        ));
    }

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
                        match accept_control_tls(acceptor, stream, config.read_timeout).await {
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
                            Err(error) => {
                                error!("TLS 握手失败，远程地址:{}，error:{}", addr, error);
                            }
                        }
                    });
                }
                Err(error) => error!("TLS 管理端口 accept 失败:{}", error),
            }
        }
    });

    let kik_listener =
        TcpListener::bind(format!("{}:{}", config.server_host, config.server_port)).await?;
    info!(
        "开启 Kik Noise 端口,监听{}的{}端口",
        config.server_host, config.server_port
    );
    loop {
        let (stream, addr) = kik_listener.accept().await?;
        let Ok(permit) = connection_limit.clone().try_acquire_owned() else {
            warn!("活动连接达到上限，拒绝 Kik Noise 连接: {}", addr);
            continue;
        };
        let acceptor = kik_noise_acceptor.clone();
        let context = context.clone();
        let config = config.clone();
        tokio::spawn(async move {
            let _permit = permit;
            match accept_kik_noise(acceptor, stream, config.read_timeout).await {
                Ok(parts) => {
                    handle_transport_parts(context, config, parts, addr, TransportPolicy::NoiseKik)
                        .await;
                }
                Err(error) => error!("Kik Noise 握手失败，远程地址:{}，error:{}", addr, error),
            }
        });
    }
}

#[derive(Clone, Copy)]
enum TransportPolicy {
    TlsControl,
    NoiseKik,
}

/// 标准 TLS record 与 RTCT 前导首字节不冲突，可在同一 TLS 专用端口接受运维探测。
fn is_tls_record_prefix(first_byte: u8) -> bool {
    first_byte == 0x16
}

/// 独立管理端口只允许 TLS，同时接受标准 ClientHello 与项目固定前导。
/// 前导不决定认证结果；两条路径最终都进入同一个 rustls TLS 1.3 acceptor。
async fn accept_control_tls(
    acceptor: ServerTlsAcceptor,
    stream: TcpStream,
    handshake_timeout: Duration,
) -> anyhow::Result<TransportParts> {
    let mut first_byte = [0_u8; 1];
    let length = timeout(handshake_timeout, stream.peek(&mut first_byte))
        .await
        .map_err(|_| anyhow::anyhow!("TLS 协议识别超时"))??;
    match first_byte.first().copied().filter(|_| length != 0) {
        Some(first) if is_tls_record_prefix(first) => {
            accept_tls(acceptor, stream, handshake_timeout).await
        }
        Some(first) if first == CTRL_TLS_PREFIX[0] => {
            accept_prefixed_tls(acceptor, stream, handshake_timeout).await
        }
        _ => Err(anyhow::anyhow!("TLS 管理端口收到未知协议")),
    }
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
                            let channel_type = channel.lock().await.channel_type;
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
    let channel_type = channel.lock().await.channel_type;
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
            let Some(id) = channel.lock().await.id().map(str::to_owned) else {
                warn!("Kik 连接关闭时尚未分配 ID");
                return;
            };
            // context.set_kik_state();
            // 因为Kik连接断开了，所以万一在被控制，需要清理
            let _ = context.delete_kik_conn_if(id.as_str(), &channel).await;
            if let Some(kik) = context.delete_kik_if_not_online(id.as_str()).await {
                info!("【{}】下线，ip:{}", kik.kik_client_info.kik_info.name, ip);
            }
        }
        ChannelType::KikData => {
            //清理
            context.delete_kik_data_conn(channel.clone()).await;
            let kik_id = channel.lock().await.attribute(&KIK_ID).cloned();
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
        let channel_type = channel.lock().await.channel_type;
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
    // 先复制枚举再进入 match，确保 Channel 锁在业务处理前释放。
    let channel_type = channel.lock().await.channel_type;
    match channel_type {
        ChannelType::Ctrl => {
            read_handle::handle_ctrl(context, channel, msg, config.security.allow_remote_exec).await
        }
        ChannelType::CtrlData => read_handle::handle_ctrl_data(context, channel, msg).await,
        ChannelType::Kik => read_handle::handle_kik(context, channel, msg).await,
        ChannelType::KikData => read_handle::handle_kik_data(context, channel, msg).await,
        ChannelType::Unknown => {
            //未识别的连接连ping pong 都不让发； unknow到其他消息状态的转换最好是同步的，不然有问题
            let allow_ctrl = matches!(transport_policy, TransportPolicy::TlsControl);
            let allow_kik = matches!(transport_policy, TransportPolicy::NoiseKik);
            read_handle::handle_init_message(config, context, channel, msg, allow_ctrl, allow_kik)
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_tls_record_prefix;

    #[test]
    fn dedicated_tls_port_recognizes_client_hello() {
        assert!(is_tls_record_prefix(0x16));
        assert!(!is_tls_record_prefix(0));
        assert!(!is_tls_record_prefix(1));
    }
}
