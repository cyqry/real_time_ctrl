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
use std::time::Duration;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::{Mutex, RwLock};
use tokio::time::timeout;
use uuid::Uuid;

const DATA_QUEUE_CAPACITY: usize = 2;
const DATA_QUEUE_SEND_TIMEOUT: Duration = Duration::from_secs(30);
const DATA_READ_TIMEOUT: Duration = Duration::from_secs(6 * 60);

pub(crate) type CommandMessage = (String, CmdOptions, Command);
pub(crate) const COMMAND_SENDER: ChannelAttributeKey<Sender<CommandMessage>> =
    ChannelAttributeKey::new(0x6b69_6b5f_636d_6473);

#[derive(Clone)]
pub struct Context {
    pub id: Arc<Mutex<Option<String>>>,
    kik_op: Arc<Mutex<Option<Kik>>>,
    data_x: DataChan,
}

#[derive(Clone)]
struct DataChan {
    tx: Sender<(String, BytesMut)>,
    rx: Arc<Mutex<Receiver<(String, BytesMut)>>>,
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
        let (tx, rx) = channel(DATA_QUEUE_CAPACITY);
        Self {
            id: Arc::new(Mutex::new(None)),
            kik_op: Arc::new(Mutex::new(None)),
            data_x: DataChan {
                tx,
                rx: Arc::new(Mutex::new(rx)),
            },
        }
    }

    pub async fn set_kik(&self, kik_op: Option<Kik>) {
        *self.kik_op.lock().await = kik_op;
    }

    pub async fn get_kik(&self) -> Option<Kik> {
        self.kik_op.lock().await.clone()
    }

    pub async fn send_data(&self, op: (String, BytesMut)) -> anyhow::Result<()> {
        timeout(DATA_QUEUE_SEND_TIMEOUT, self.data_x.tx.send(op))
            .await
            .map_err(|_| anyhow::Error::msg(hidden!("数据接收队列持续拥塞")))?
            .context(hidden!("数据接收者已关闭"))?;
        Ok(())
    }

    pub async fn read_data(&self, key: &str) -> anyhow::Result<BytesMut> {
        timeout(DATA_READ_TIMEOUT, async {
            loop {
                let (id, data) = self
                    .data_x
                    .rx
                    .lock()
                    .await
                    .recv()
                    .await
                    .ok_or_else(|| anyhow::Error::msg(hidden!("数据接收通道已关闭")))?;
                if id == key {
                    return Ok(data);
                }
                // 连接重建时可能残留旧请求数据；丢弃不匹配帧，不能让它污染当前命令。
            }
        })
        .await
        .map_err(|_| anyhow::Error::msg(hidden!("数据读取超时")))?
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

    pub async fn find_and_send_data(&self, data: &[u8]) -> anyhow::Result<String> {
        let data_id = Uuid::new_v4().to_string();
        self.send_data_with_id(&data_id, data).await?;
        Ok(data_id)
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
        let kik = self.kik_op.lock().await.clone();
        if let Some(kik) = kik {
            kik.clear().await;
        }
    }
}

#[tokio::test]
async fn data_channel_safely_skips_stale_frame() {
    let context = Context::new();
    context
        .send_data(("stale".to_string(), BytesMut::from(&b"old"[..])))
        .await
        .unwrap();
    context
        .send_data(("wanted".to_string(), BytesMut::from(&b"new"[..])))
        .await
        .unwrap();

    assert_eq!(context.read_data("wanted").await.unwrap(), b"new"[..]);
}
