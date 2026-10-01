//! 服务端监听、传输握手、帧读取和连接清理主循环。
//!
//! 两个公网端口先分别完成 TLS 或 Noise，再统一包装为 `Channel + FramedRead`。初始化处理器确定角色后，
//! 读循环才放宽帧上限并分派到 control/data/kik 处理器；任何退出路径最终进入 `handle_inactive` 清理状态。

use crate::core::connection_meta::KIK_ID;
use crate::core::context::Context;
use crate::handler::read_handle;
use anyhow::Error;
use bytes::BytesMut;
use common::channel::{Channel, ChannelType, DATA_CHANNEL_IO_TIMEOUT};
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
use log::{debug, error, info, warn};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinSet;
use tokio::time::timeout;
use tokio::{io, time};
use tokio_stream::StreamExt;
use tokio_util::codec::FramedRead;

pub async fn run(context: Context, config: Config) -> anyhow::Result<()> {
    // `run` 只在这里创建 listener；后续每个 accept 和连接读循环都在独立任务中运行。
    // 两个公网端口分别限流。Kik 是匿名 Noise 发起方，若与管理面共享许可，攻击者可仅靠
    // 占满 Kik 慢握手使合法 TLS 控制端无法接入；独立上限同时保证隔离和总资源可计算。
    const MAX_TLS_CONNECTIONS: usize = 512;
    const MAX_KIK_CONNECTIONS: usize = 512;
    let tls_connection_limit = Arc::new(Semaphore::new(MAX_TLS_CONNECTIONS));
    let kik_connection_limit = Arc::new(Semaphore::new(MAX_KIK_CONNECTIONS));
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
    let tls_connection_limit = tls_connection_limit.clone();
    tokio::spawn(async move {
        loop {
            match tls_listener.accept().await {
                Ok((stream, addr)) => {
                    let Ok(permit) = tls_connection_limit.clone().try_acquire_owned() else {
                        // 远端可控事件不能逐条写 production 日志，否则连接风暴会放大为磁盘 DoS。
                        debug!("TLS 活动连接达到上限，拒绝连接: {}", addr);
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
                                debug!("TLS 握手失败，远程地址:{}，error:{}", addr, error);
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
        let Ok(permit) = kik_connection_limit.clone().try_acquire_owned() else {
            debug!("Kik Noise 活动连接达到上限，拒绝连接: {}", addr);
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
                Err(error) => debug!("Kik Noise 握手失败，远程地址:{}，error:{}", addr, error),
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

    let mut heartbeat_task = JoinSet::new();
    heartbeat_task.spawn(heartbeat(channel.clone()));

    let e = loop {
        // `FramedRead::next` 只有在整帧到齐后才返回。4 MiB 文件帧不能沿用控制通道的 45 秒
        // 阈值，否则慢公网会把仍在接收的健康连接误判为静默连接。
        let frame_read_timeout = channel
            .lock()
            .await
            .channel_type
            .frame_read_timeout(config.read_timeout);
        // 读锁必须在进入 match 前释放；否则后续调整 decoder 上限会再次锁同一个 FramedRead。
        let read_result = {
            let mut framed = framed_arc.lock().await;
            tokio::select! {
                biased;
                // 写端已不可用时不再等待读超时；即使 shutdown 卡住，也会在其有界收尾后退出。
                _ = heartbeat_task.join_next() => break None,
                result = timeout(frame_read_timeout, framed.next()) => result,
            }
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
                            if matches!(channel_type, ChannelType::CtrlData | ChannelType::KikData)
                            {
                                channel
                                    .lock()
                                    .await
                                    .set_write_timeout(DATA_CHANNEL_IO_TIMEOUT);
                            }
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
    // 读循环先退出时也必须取消、回收心跳；JoinSet 还覆盖外层任务被取消的情况。
    heartbeat_task.shutdown().await;
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
        debug!("连接 I/O 结束，远程地址:{}，error:{}", remote_addr, error);
    } else if error.is::<time::error::Elapsed>() {
        debug!("连接读取超时，远程地址:{}", remote_addr);
    } else {
        debug!("拒绝无效连接，远程地址:{}，error:{}", remote_addr, error);
    }
}

async fn handle_inactive(context: Context, channel: Arc<Mutex<Channel>>) {
    // 所有退出原因共用同一清理入口；先关闭写半边，阻止其他任务继续复用该流。
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
            context.delete_ctrl_data_conn(channel).await;
        }
        ChannelType::Kik => {
            // Kik 主连接的 Channel ID 就是 Kik ID；指针检查避免旧连接回调删掉重连后的新连接。
            let Some(id) = channel.lock().await.id().map(str::to_owned) else {
                warn!("Kik 连接关闭时尚未分配 ID");
                return;
            };
            let _ = context.delete_kik_conn_if(id.as_str(), &channel).await;
            // 只有主连接和全部数据连接都消失，才把 Kik 记为完整下线。
            if let Some(kik) = context.delete_kik_if_not_online(id.as_str()).await {
                info!("【{}】下线，ip:{}", kik.kik_client_info.kik_info.name, ip);
            }
        }
        ChannelType::KikData => {
            context.delete_kik_data_conn(channel.clone()).await;
            let kik_id = channel.lock().await.attribute(&KIK_ID).cloned();
            if let Some(kik_id) = kik_id {
                if let Some(kik) = context.delete_kik_if_not_online(&kik_id).await {
                    info!("【{}】下线，ip:{}", kik.kik_client_info.kik_info.name, ip);
                }
            }
        }
        // 初始化失败的连接尚未进入任何全局会话表，关闭网络流即可。
        ChannelType::Unknown => {}
    };
}

async fn heartbeat(channel: Arc<Mutex<Channel>>) {
    loop {
        // 初始化阶段不能发送业务帧；先等待角色确定，再按该角色选择心跳编码。
        time::sleep(Duration::from_secs(5)).await;
        let channel_type = {
            let mut guard = channel.lock().await;
            if guard.is_closed() {
                guard.try_write_half_close().await;
                return;
            }
            guard.channel_type
        };
        let ping = match channel_type {
            ChannelType::Ctrl | ChannelType::CtrlData => Some(ctrl_ping()),
            ChannelType::Kik | ChannelType::KikData => Some(kik_ping()),
            ChannelType::Unknown => None,
        };
        let Some(ping) = ping else { continue };

        // 角色刚切换时再留一个间隔，让初始化确认先到达客户端，避免确认与心跳交错。
        time::sleep(Duration::from_secs(5)).await;
        let mut channel = channel.lock().await;
        match channel.write_and_flush(&ping).await {
            Ok(_) => {}
            Err(_) => {
                // 尽力通知对端；本地读循环也监听心跳任务结束，不依赖对端一定返回 EOF。
                channel.try_write_half_close().await;
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

/// 根据已经确定的连接角色分派一条完整帧。
///
/// `Unknown` 阶段只允许初始化消息；端口传输策略进一步限制 TLS 只能声明 Ctrl/CtrlData、Noise 只能
/// 声明 Kik/KikData。角色切换在初始化处理器返回前完成，因此下一帧会稳定进入业务处理器。
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
            let allow_ctrl = matches!(transport_policy, TransportPolicy::TlsControl);
            let allow_kik = matches!(transport_policy, TransportPolicy::NoiseKik);
            read_handle::handle_init_message(config, context, channel, msg, allow_ctrl, allow_kik)
                .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::config::{Id, SecurityConfig};
    use common::kik_info::KikInfo;
    use common::message::init_frame::InitFrame;
    use common::protocol::transfer_encode_frame;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context as TaskContext, Poll};
    use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use tokio::sync::Notify;

    #[test]
    fn dedicated_tls_port_recognizes_client_hello() {
        assert!(is_tls_record_prefix(0x16));
        assert!(!is_tls_record_prefix(0));
        assert!(!is_tls_record_prefix(1));
    }

    /// 初始化响应成功后只让心跳写入失败；shutdown 始终 Pending，模拟加密层无法刷出收尾数据。
    struct FailingHeartbeatWriter {
        writes: Arc<AtomicUsize>,
        shutdowns: Arc<AtomicUsize>,
        initialized: Arc<Notify>,
        dropped: Arc<Notify>,
    }

    impl AsyncWrite for FailingHeartbeatWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut TaskContext<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            if self.writes.fetch_add(1, Ordering::SeqCst) == 0 {
                self.initialized.notify_one();
                Poll::Ready(Ok(bytes.len()))
            } else {
                Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe)))
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
            Poll::Pending
        }
    }

    impl Drop for FailingHeartbeatWriter {
        fn drop(&mut self) {
            self.dropped.notify_one();
        }
    }

    #[tokio::test]
    async fn heartbeat_failure_releases_blocked_real_read_loop() {
        let writes = Arc::new(AtomicUsize::new(0));
        let shutdowns = Arc::new(AtomicUsize::new(0));
        let initialized = Arc::new(Notify::new());
        let dropped = Arc::new(Notify::new());
        let (reader, mut peer) = tokio::io::duplex(1024);
        let addr = "127.0.0.1:1".parse().unwrap();
        let parts = TransportParts {
            reader: Box::pin(reader),
            writer: Box::pin(FailingHeartbeatWriter {
                writes: writes.clone(),
                shutdowns: shutdowns.clone(),
                initialized: initialized.clone(),
                dropped: dropped.clone(),
            }),
            local_addr: Ok(addr),
            peer_addr: Ok(addr),
        };
        let config = Config {
            id: Id::anonymous(),
            server_host: String::new(),
            server_port: String::new(),
            read_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_millis(50),
            security: SecurityConfig::kik(),
        };
        // 直接运行生产读循环；只替换传输字节流，不复制 select 或清理逻辑。
        let mut connection = tokio::spawn(handle_transport_parts(
            Context::init(),
            config,
            parts,
            addr,
            TransportPolicy::NoiseKik,
        ));
        peer.write_all(&transfer_encode_frame(InitFrame::KikReq(KikInfo {
            id: None,
            name: "heartbeat-regression".to_string(),
        })))
        .await
        .unwrap();
        timeout(Duration::from_secs(2), initialized.notified())
            .await
            .expect("初始化响应应成功，随后读循环等待读取下一帧");
        assert!(!connection.is_finished());

        // 保持 peer 打开且不再发送。初始化原有 5 秒登记延迟，心跳也按 5+5 秒节奏运行，
        // 故给首轮最多 15 秒并留调度余量；旧实现仍只能等待 60 秒读超时。
        let result = timeout(Duration::from_secs(18), &mut connection).await;
        if result.is_err() {
            connection.abort();
            let _ = connection.await;
        }
        result.expect("写心跳失败必须提前结束真实读循环").unwrap();
        timeout(Duration::from_secs(2), dropped.notified())
            .await
            .expect("inactive 收尾后必须释放故障 writer");
        assert_eq!(writes.load(Ordering::SeqCst), 2);
        assert!(shutdowns.load(Ordering::SeqCst) >= 1);
        assert!(peer.write_all(b"late-frame").await.is_err());
    }

    #[tokio::test]
    async fn heartbeat_closes_channel_after_cancelled_frame() {
        let (writer, mut peer) = tokio::io::duplex(1);
        let addr = "127.0.0.1:1".parse().unwrap();
        let channel = Arc::new(Mutex::new(Channel::new(
            Box::pin(writer),
            None,
            ChannelType::CtrlData,
            Ok(addr),
            Ok(addr),
        )));
        {
            let mut guard = channel.lock().await;
            // duplex 只容纳一个字节，整帧写入会阻塞；外层 timeout 模拟业务 future 被取消。
            assert!(timeout(
                Duration::from_millis(20),
                guard.write_and_flush(b"partial-frame")
            )
            .await
            .is_err());
            assert!(guard.is_closed());
        }
        timeout(Duration::from_secs(7), heartbeat(channel.clone()))
            .await
            .expect("已失效连接应在下一轮心跳有界关闭");
        let mut received = Vec::new();
        timeout(Duration::from_secs(1), peer.read_to_end(&mut received))
            .await
            .expect("心跳关闭必须让对端读到 EOF，而不是只留下 closed 标志")
            .unwrap();
        assert_eq!(received, b"p");
        assert!(channel.lock().await.is_closed());
    }
}
