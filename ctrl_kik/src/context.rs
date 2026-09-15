//! ctrl_kik 单进程内的主连接、数据连接和按数据 ID 隔离的收件箱。
//!
//! 控制命令与文件数据来自不同 TCP/Noise 连接，先后顺序没有保证。`Context` 因此允许数据先有界
//! 预登记，命令到达后再认领；每个 ID 使用自己的容量受限队列，避免并行上传串流。

use anyhow::Context as AnyhowContext;
use bytes::BytesMut;
use common::channel::{Channel, ChannelAttributeKey};
use common::command::Command;
use common::hidden;
use common::message::kik_frame::encode_data_frame;
use common::protocol::CmdOptions;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::{Mutex, Notify, RwLock};
use tokio::time::timeout;

const DATA_QUEUE_CAPACITY: usize = 2;
const DATA_QUEUE_SEND_TIMEOUT: Duration = Duration::from_secs(30);
const DATA_READ_TIMEOUT: Duration = Duration::from_secs(6 * 60);
const PRE_REGISTERED_ROUTE_TTL: Duration = Duration::from_secs(30);
const MAX_DATA_ROUTES: usize = 64;
const MAX_PRE_REGISTERED_ROUTES: usize = 16;
const MAX_PRE_REGISTERED_BYTES: usize = 64 * 1024 * 1024;

pub(crate) type CommandMessage = (String, CmdOptions, Command);
pub(crate) const COMMAND_SENDER: ChannelAttributeKey<Sender<CommandMessage>> =
    ChannelAttributeKey::new(0x6b69_6b5f_636d_6473);

#[derive(Clone)]
/// 当前 Kik 生命周期共享的状态；主连接断开时整体清空。
pub struct Context {
    /// 服务端分配的 Kik ID，数据连接初始化时需要读取。
    pub id: Arc<Mutex<Option<String>>>,
    /// 当前主会话及其数据连接池。
    kik_op: Arc<Mutex<Option<Kik>>>,
    /// 上传数据 ID 到私有队列的映射。
    data_routes: Arc<Mutex<HashMap<String, DataChan>>>,
}

/// 一个数据 ID 的有界收件箱。
struct DataChan {
    /// KikData 读循环持有的生产端。
    tx: Sender<(String, BytesMut)>,
    /// 唯一命令任务持有的消费端。
    rx: Arc<Mutex<Receiver<(String, BytesMut)>>>,
    /// false 表示数据先于命令到达，只能在短 TTL 和总内存上限内暂存。
    registered: bool,
    created_at: Instant,
    /// 未注册阶段累计的 payload 大小，认领后归零。
    pre_registered_bytes: usize,
}

#[derive(Clone)]
/// 一条 Kik 主连接及其多条 KikData 连接。
pub struct Kik {
    /// 主连接；清理时先从 Option 移出，再在锁外关闭。
    pub conn_op: Arc<RwLock<Option<Arc<Mutex<Channel>>>>>,
    /// 连接自身随机 ID 到写半连接的映射。
    data_conns: Arc<Mutex<HashMap<String, Arc<Mutex<Channel>>>>>,
    /// 某个数据连接槽位重建成功时唤醒正在等待继续发送的文件任务。
    data_connection_notify: Arc<Notify>,
    /// 多连接发送的轮询游标，不参与授权。
    next_data_conn: Arc<AtomicUsize>,
}

impl Kik {
    pub fn new(channel: Arc<Mutex<Channel>>) -> Self {
        Self {
            conn_op: Arc::new(RwLock::new(Some(channel))),
            data_conns: Arc::new(Mutex::new(HashMap::new())),
            data_connection_notify: Arc::new(Notify::new()),
            next_data_conn: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// 判断两个句柄是否指向同一轮主连接。
    ///
    /// 服务端可能给重连后的进程继续分配同一个 Kik ID，所以字符串 ID 不能区分新旧生命周期；
    /// 使用主连接状态的 `Arc` 身份可阻止旧握手任务误加入新一轮连接池。
    pub fn same_session(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.conn_op, &other.conn_op)
    }

    pub async fn insert_data_conn(&self, data_chan: Arc<Mutex<Channel>>) -> anyhow::Result<()> {
        let id = data_chan.lock().await.require_id()?.to_string();
        self.data_conns.lock().await.insert(id, data_chan);
        // 连接池从空恢复时可能有多个并发文件任务在等待；所有任务都应立即重新检查连接池。
        self.data_connection_notify.notify_waiters();
        Ok(())
    }

    pub async fn delete_data_conn(&self, conn: Arc<Mutex<Channel>>) -> Option<Arc<Mutex<Channel>>> {
        let id = conn.lock().await.id().map(str::to_owned)?;
        self.data_conns.lock().await.remove(&id)
    }

    async fn data_connections_for_send(&self) -> Vec<Arc<Mutex<Channel>>> {
        let data_map = self.data_conns.lock().await;
        let count = data_map.len();
        if count == 0 {
            return Vec::new();
        }
        let start = self.next_data_conn.fetch_add(1, Ordering::Relaxed) % count;
        data_map
            .values()
            .cycle()
            .skip(start)
            .take(count)
            .cloned()
            .collect()
    }

    /// 等待当前会话至少出现一条健康数据连接。
    ///
    /// 连接监督器会在断线后自动补建。发送任务在此保留尚未发送的完整帧，等待恢复后重试，
    /// 而不是立刻让整次大文件传输失败；调用方提供硬超时，避免永久等待。
    async fn wait_data_connections_for_send(
        &self,
        wait_timeout: Duration,
    ) -> Vec<Arc<Mutex<Channel>>> {
        let deadline = Instant::now() + wait_timeout;
        loop {
            let notified = self.data_connection_notify.notified();
            tokio::pin!(notified);
            // `notified()` 只有被轮询后才会真正进入等待队列。先 enable 再检查连接池，才能让
            // notify_waiters 覆盖“检查为空”之前已创建、但尚未 await 的等待者。
            notified.as_mut().enable();
            let candidates = self.data_connections_for_send().await;
            let mut healthy = Vec::with_capacity(candidates.len());
            for connection in candidates {
                if !connection.lock().await.is_closed() {
                    healthy.push(connection);
                }
            }
            if !healthy.is_empty() {
                return healthy;
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || timeout(remaining, notified.as_mut()).await.is_err() {
                return Vec::new();
            }
        }
    }

    pub async fn delete_kik_conn(&self) -> Option<Arc<Mutex<Channel>>> {
        self.conn_op.write().await.take()
    }

    pub async fn clear(&self) {
        let data_connections = {
            let mut guard = self.data_conns.lock().await;
            guard
                .drain()
                .map(|(_, channel)| channel)
                .collect::<Vec<_>>()
        };
        for connection in data_connections {
            connection.lock().await.try_write_half_close().await;
        }

        if let Some(connection) = self.conn_op.write().await.take() {
            connection.lock().await.try_write_half_close().await;
        }
    }
}

impl Context {
    pub fn new() -> Self {
        // 有界队列把背压传回 TCP 读循环，避免对端持续发送大帧时无限占用内存。
        Self {
            id: Arc::new(Mutex::new(None)),
            kik_op: Arc::new(Mutex::new(None)),
            data_routes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn set_kik(&self, kik_op: Option<Kik>) {
        *self.kik_op.lock().await = kik_op;
    }

    pub async fn get_kik(&self) -> Option<Kik> {
        self.kik_op.lock().await.clone()
    }

    /// 投递一帧上传数据；不存在路由时创建严格受限的预登记收件箱。
    pub async fn send_data(&self, op: (String, BytesMut)) -> anyhow::Result<()> {
        let (sender, cleanup) = {
            let mut routes = self.data_routes.lock().await;
            let now = Instant::now();
            routes.retain(|_, route| {
                route.registered
                    || now.saturating_duration_since(route.created_at) <= PRE_REGISTERED_ROUTE_TTL
            });

            let current_pre_registered_bytes = routes
                .values()
                .filter(|route| !route.registered)
                .map(|route| route.pre_registered_bytes)
                .sum::<usize>();
            if let Some(route) = routes.get_mut(&op.0) {
                if !route.registered {
                    let new_total = current_pre_registered_bytes.saturating_add(op.1.len());
                    if new_total > MAX_PRE_REGISTERED_BYTES {
                        return Err(anyhow::Error::msg(hidden!("预到达数据缓冲达到上限")));
                    }
                    route.pre_registered_bytes =
                        route.pre_registered_bytes.saturating_add(op.1.len());
                }
                (route.tx.clone(), None)
            } else {
                let pre_registered_count =
                    routes.values().filter(|route| !route.registered).count();
                if routes.len() >= MAX_DATA_ROUTES
                    || pre_registered_count >= MAX_PRE_REGISTERED_ROUTES
                    || current_pre_registered_bytes.saturating_add(op.1.len())
                        > MAX_PRE_REGISTERED_BYTES
                {
                    return Err(anyhow::Error::msg(hidden!("预到达数据路由达到上限")));
                }
                let key = op.0.clone();
                let created_at = Instant::now();
                let (tx, rx) = channel(DATA_QUEUE_CAPACITY);
                routes.insert(
                    key.clone(),
                    DataChan {
                        tx: tx.clone(),
                        rx: Arc::new(Mutex::new(rx)),
                        registered: false,
                        created_at,
                        pre_registered_bytes: op.1.len(),
                    },
                );
                (tx, Some((key, created_at)))
            }
        };
        if let Some((key, created_at)) = cleanup {
            let routes = self.data_routes.clone();
            tokio::spawn(async move {
                tokio::time::sleep(PRE_REGISTERED_ROUTE_TTL).await;
                let mut routes = routes.lock().await;
                if routes
                    .get(&key)
                    .is_some_and(|route| !route.registered && route.created_at == created_at)
                {
                    routes.remove(&key);
                }
            });
        }
        timeout(DATA_QUEUE_SEND_TIMEOUT, sender.send(op))
            .await
            .map_err(|_| anyhow::Error::msg(hidden!("数据接收队列持续拥塞")))?
            .context(hidden!("数据接收者已关闭"))?;
        Ok(())
    }

    /// 从已认领的数据 ID 收件箱读取下一帧，供小文件或大文件循环消费。
    pub async fn read_data(&self, key: &str) -> anyhow::Result<BytesMut> {
        let receiver = self
            .data_routes
            .lock()
            .await
            .get(key)
            .filter(|route| route.registered)
            .map(|route| route.rx.clone())
            .ok_or_else(|| anyhow::Error::msg(hidden!("数据接收路由未登记")))?;
        timeout(DATA_READ_TIMEOUT, async {
            let (_, data) = receiver
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| anyhow::Error::msg(hidden!("数据接收通道已关闭")))?;
            Ok(data)
        })
        .await
        .map_err(|_| anyhow::Error::msg(hidden!("数据读取超时")))?
    }

    /// 命令通道与数据通道是独立 TCP 流，公网中数据帧可能合法地先到达。此处把短期预到达
    /// 路由升级为已登记路由；预到达数量、总字节数和存活时间均有硬上限。
    /// 命令到达时认领或创建数据 ID；重复活动命令使用同一 ID 会被拒绝。
    pub async fn register_data_route(&self, key: &str) -> anyhow::Result<()> {
        let mut routes = self.data_routes.lock().await;
        if let Some(route) = routes.get_mut(key) {
            if route.registered {
                return Err(anyhow::Error::msg(hidden!("数据接收路由重复")));
            }
            route.registered = true;
            route.pre_registered_bytes = 0;
            return Ok(());
        }
        if routes.len() >= MAX_DATA_ROUTES {
            return Err(anyhow::Error::msg(hidden!("活动数据传输达到上限")));
        }
        let (tx, rx) = channel(DATA_QUEUE_CAPACITY);
        let rx = Arc::new(Mutex::new(rx));
        routes.insert(
            key.to_string(),
            DataChan {
                tx: tx.clone(),
                rx: rx.clone(),
                registered: true,
                created_at: Instant::now(),
                pre_registered_bytes: 0,
            },
        );
        Ok(())
    }

    pub async fn remove_data_route(&self, key: &str) {
        self.data_routes.lock().await.remove(key);
    }

    /// 仅把数据连接加入创建它的那一轮 Kik 主会话。
    pub async fn insert_data_conn_for(
        &self,
        expected: &Kik,
        conn: Arc<Mutex<Channel>>,
    ) -> anyhow::Result<()> {
        let current = self
            .kik_op
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow::Error::msg(hidden!("命令通道尚未初始化")))?;
        if !current.same_session(expected) {
            return Err(anyhow::Error::msg(hidden!("数据连接属于已结束的命令会话")));
        }
        expected.insert_data_conn(conn).await
    }

    /// 供数据连接监督器判断主连接是否已经换代。
    pub async fn is_current_kik(&self, expected: &Kik) -> bool {
        self.kik_op
            .lock()
            .await
            .as_ref()
            .is_some_and(|current| current.same_session(expected))
    }

    /// 将下载 payload 编成完整帧，并在数据连接快照内轮询/失败换路。
    pub async fn send_data_with_id(&self, data_id: &str, data: &[u8]) -> anyhow::Result<()> {
        let encoded = encode_data_frame(data_id, data)?;
        let kik = self
            .kik_op
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow::Error::msg(hidden!("命令通道尚未初始化")))?;
        let deadline = Instant::now() + common::channel::DATA_CONNECTION_RECOVERY_TIMEOUT;
        let mut last_error = None;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let connections = kik.wait_data_connections_for_send(remaining).await;
            if connections.is_empty() {
                break;
            }
            for connection in connections {
                let mut connection = connection.lock().await;
                match connection.write_and_flush(&encoded).await {
                    Ok(()) => return Ok(()),
                    Err(error) => {
                        // 半帧连接不可复用；主动关闭会促使监督器尽快补建该槽位。
                        connection.try_write_half_close().await;
                        last_error = Some(error);
                    }
                }
            }
        }
        Err(last_error
            .unwrap_or_else(|| anyhow::Error::msg(hidden!("Kik数据连接在恢复时限内不可用"))))
    }

    pub async fn clear(&self) {
        self.data_routes.lock().await.clear();
        let kik = self.kik_op.lock().await.clone();
        if let Some(kik) = kik {
            kik.clear().await;
        }
    }
}

#[tokio::test]
async fn data_channels_are_isolated_by_id() {
    let context = Context::new();
    context.register_data_route("stale").await.unwrap();
    context.register_data_route("wanted").await.unwrap();
    context
        .send_data(("stale".to_string(), BytesMut::from(&b"old"[..])))
        .await
        .unwrap();
    context
        .send_data(("wanted".to_string(), BytesMut::from(&b"new"[..])))
        .await
        .unwrap();

    assert_eq!(context.read_data("wanted").await.unwrap(), b"new"[..]);
    assert_eq!(context.read_data("stale").await.unwrap(), b"old"[..]);
}

#[tokio::test]
async fn data_may_arrive_before_its_command_on_an_independent_connection() {
    let context = Context::new();
    context
        .send_data((
            "unsolicited-route-test-id".to_string(),
            BytesMut::from(&b"unsolicited-payload-test-value"[..]),
        ))
        .await
        .unwrap();
    context
        .register_data_route("unsolicited-route-test-id")
        .await
        .unwrap();
    assert_eq!(
        context
            .read_data("unsolicited-route-test-id")
            .await
            .unwrap(),
        b"unsolicited-payload-test-value"[..]
    );
    context.remove_data_route("unsolicited-route-test-id").await;
}

#[tokio::test]
async fn sender_waits_for_a_recovered_data_connection() {
    let context = Context::new();
    let (_main_peer, main_stream) = tokio::io::duplex(4096);
    let main_channel = Arc::new(Mutex::new(Channel::new(
        Box::pin(main_stream),
        Some("kik-test".to_string()),
        common::channel::ChannelType::Kik,
        Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "test",
        )),
        Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "test",
        )),
    )));
    let kik = Kik::new(main_channel);
    context.set_kik(Some(kik.clone())).await;

    let send_context = context.clone();
    let send_task = tokio::spawn(async move {
        send_context
            .send_data_with_id("recovery-data-id", b"payload")
            .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        !send_task.is_finished(),
        "没有连接时发送任务应等待监督器恢复"
    );

    let (_data_peer, data_stream) = tokio::io::duplex(4096);
    let data_channel = Arc::new(Mutex::new(Channel::new(
        Box::pin(data_stream),
        Some("data-connection".to_string()),
        common::channel::ChannelType::KikData,
        Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "test",
        )),
        Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "test",
        )),
    )));
    context
        .insert_data_conn_for(&kik, data_channel)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), send_task)
        .await
        .expect("数据连接恢复后发送任务应立即继续")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn pool_recovery_wakes_all_concurrent_waiters() {
    let (_main_peer, main_stream) = tokio::io::duplex(4096);
    let main_channel = Arc::new(Mutex::new(Channel::new(
        Box::pin(main_stream),
        Some("kik-broadcast-test".to_string()),
        common::channel::ChannelType::Kik,
        Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "test",
        )),
        Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "test",
        )),
    )));
    let kik = Kik::new(main_channel);

    // 模拟多个并行文件请求同时遇到整个数据池离线。恢复一条连接后，每个请求都应立刻
    // 重新检查共享池，而不是只有一个请求被唤醒、其余请求一直等待到恢复超时。
    let mut waiters = Vec::new();
    for _ in 0..8 {
        let waiting_kik = kik.clone();
        waiters.push(tokio::spawn(async move {
            waiting_kik
                .wait_data_connections_for_send(Duration::from_secs(2))
                .await
                .len()
        }));
    }
    tokio::time::sleep(Duration::from_millis(20)).await;

    let (_data_peer, data_stream) = tokio::io::duplex(4096);
    let data_channel = Arc::new(Mutex::new(Channel::new(
        Box::pin(data_stream),
        Some("recovered-data-connection".to_string()),
        common::channel::ChannelType::KikData,
        Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "test",
        )),
        Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "test",
        )),
    )));
    assert!(kik.insert_data_conn(data_channel).await.is_ok());

    tokio::time::timeout(Duration::from_millis(500), async {
        for waiter in waiters {
            assert_eq!(waiter.await.unwrap(), 1);
        }
    })
    .await
    .expect("连接池恢复应广播唤醒所有并发等待者");
}

#[tokio::test]
async fn stale_data_handshake_cannot_join_a_new_main_session() {
    let context = Context::new();
    let (_old_peer, old_stream) = tokio::io::duplex(64);
    let old_kik = Kik::new(Arc::new(Mutex::new(Channel::new(
        Box::pin(old_stream),
        Some("same-kik-id".to_string()),
        common::channel::ChannelType::Kik,
        Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "test",
        )),
        Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "test",
        )),
    ))));
    let (_new_peer, new_stream) = tokio::io::duplex(64);
    let new_kik = Kik::new(Arc::new(Mutex::new(Channel::new(
        Box::pin(new_stream),
        Some("same-kik-id".to_string()),
        common::channel::ChannelType::Kik,
        Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "test",
        )),
        Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "test",
        )),
    ))));
    context.set_kik(Some(new_kik)).await;

    let (_data_peer, data_stream) = tokio::io::duplex(64);
    let stale_data = Arc::new(Mutex::new(Channel::new(
        Box::pin(data_stream),
        Some("late-old-data".to_string()),
        common::channel::ChannelType::KikData,
        Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "test",
        )),
        Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "test",
        )),
    )));
    assert!(
        context
            .insert_data_conn_for(&old_kik, stale_data)
            .await
            .is_err(),
        "相同字符串 ID 不能让旧会话数据连接混入新连接池"
    );
}
