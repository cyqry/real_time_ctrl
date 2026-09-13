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
use tokio::sync::{Mutex, RwLock};
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
pub struct Context {
    pub id: Arc<Mutex<Option<String>>>,
    kik_op: Arc<Mutex<Option<Kik>>>,
    data_routes: Arc<Mutex<HashMap<String, DataChan>>>,
}

struct DataChan {
    tx: Sender<(String, BytesMut)>,
    rx: Arc<Mutex<Receiver<(String, BytesMut)>>>,
    registered: bool,
    created_at: Instant,
    pre_registered_bytes: usize,
}

#[derive(Clone)]
pub struct Kik {
    pub conn_op: Arc<RwLock<Option<Arc<Mutex<Channel>>>>>,
    data_conns: Arc<Mutex<HashMap<String, Arc<Mutex<Channel>>>>>,
    next_data_conn: Arc<AtomicUsize>,
}

impl Kik {
    pub fn new(channel: Arc<Mutex<Channel>>) -> Self {
        Self {
            conn_op: Arc::new(RwLock::new(Some(channel))),
            data_conns: Arc::new(Mutex::new(HashMap::new())),
            next_data_conn: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub async fn insert_data_conn(&self, data_chan: Arc<Mutex<Channel>>) -> anyhow::Result<()> {
        let id = data_chan.lock().await.require_id()?.to_string();
        self.data_conns.lock().await.insert(id, data_chan);
        Ok(())
    }

    pub async fn delete_data_conn(&self, conn: Arc<Mutex<Channel>>) -> Option<Arc<Mutex<Channel>>> {
        let id = conn.lock().await.id().map(str::to_owned)?;
        self.data_conns.lock().await.remove(&id)
    }

    pub async fn find_data_conn(&self) -> Option<Arc<Mutex<Channel>>> {
        self.data_connections_for_send().await.into_iter().next()
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

    pub async fn insert_data_conn(&self, conn: Arc<Mutex<Channel>>) -> anyhow::Result<()> {
        let kik = self
            .kik_op
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow::Error::msg(hidden!("命令通道尚未初始化")))?;
        kik.insert_data_conn(conn).await?;
        Ok(())
    }

    pub async fn delete_data_conn(&self, conn: Arc<Mutex<Channel>>) {
        if let Some(kik) = self.kik_op.lock().await.clone() {
            kik.delete_data_conn(conn).await;
        }
    }

    pub async fn find_data_conn(&self) -> Option<Arc<Mutex<Channel>>> {
        let kik = self.kik_op.lock().await.clone();
        match kik {
            Some(kik) => kik.find_data_conn().await,
            None => None,
        }
    }

    pub async fn send_data_with_id(&self, data_id: &str, data: &[u8]) -> anyhow::Result<()> {
        let encoded = encode_data_frame(data_id, data)?;
        let kik = self
            .kik_op
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow::Error::msg(hidden!("命令通道尚未初始化")))?;
        let mut last_error = None;
        for connection in kik.data_connections_for_send().await {
            let mut connection = connection.lock().await;
            if connection.is_closed() {
                continue;
            }
            match connection.write_and_flush(&encoded).await {
                Ok(()) => return Ok(()),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::Error::msg(hidden!("Kik数据连接未初始化完成"))))
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
