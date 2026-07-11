use anyhow::{anyhow, Context as AnyhowContext};
use bytes::{BufMut, BytesMut};
use common::channel::Channel;
use common::message::kik_frame::KikFrame;
use common::protocol;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::{Mutex, RwLock};
use tokio::time::timeout;
use uuid::Uuid;

const DATA_QUEUE_CAPACITY: usize = 8;
const DATA_QUEUE_SEND_TIMEOUT: Duration = Duration::from_secs(30);
const DATA_READ_TIMEOUT: Duration = Duration::from_secs(6 * 60);

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

    pub async fn insert_data_conn(&self, data_chan: Arc<Mutex<Channel>>) {
        let id = data_chan.lock().await.get_id().to_string();
        self.data_conns.lock().await.insert(id, data_chan);
    }

    pub async fn delete_data_conn(&self, conn: Arc<Mutex<Channel>>) -> Option<Arc<Mutex<Channel>>> {
        let id = conn.lock().await.get_id().to_string();
        self.data_conns.lock().await.remove(&id)
    }

    pub async fn find_data_conn(&self) -> Option<Arc<Mutex<Channel>>> {
        let data_map = self.data_conns.lock().await;
        if data_map.is_empty() {
            return None;
        }
        let next = self.next_data_conn.fetch_add(1, Ordering::Relaxed) % data_map.len();
        data_map.values().nth(next).cloned()
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
            .map_err(|_| anyhow!("数据接收队列持续拥塞"))?
            .context("数据接收者已关闭")?;
        Ok(())
    }

    pub async fn read_data(&self, key: String) -> anyhow::Result<BytesMut> {
        timeout(DATA_READ_TIMEOUT, async {
            loop {
                let (id, data) = self
                    .data_x
                    .rx
                    .lock()
                    .await
                    .recv()
                    .await
                    .ok_or_else(|| anyhow!("数据接收通道已关闭"))?;
                if id == key {
                    return Ok(data);
                }
                // 连接重建时可能残留旧请求数据；丢弃不匹配帧，不能让它污染当前命令。
            }
        })
        .await
        .map_err(|_| anyhow!("数据读取超时"))?
    }

    pub async fn insert_data_conn(&self, conn: Arc<Mutex<Channel>>) -> anyhow::Result<()> {
        let kik = self
            .kik_op
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("命令通道尚未初始化"))?;
        kik.insert_data_conn(conn).await;
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
        let connection = self
            .find_data_conn()
            .await
            .ok_or_else(|| anyhow!("Kik数据连接未初始化完成"))?;
        let data_id = Uuid::new_v4().to_string();
        let mut bytes = BytesMut::with_capacity(data.len());
        bytes.put_slice(data);
        connection
            .lock()
            .await
            .write_and_flush(&protocol::transfer_encode_frame(KikFrame::Data(
                data_id.clone(),
                bytes,
            )))
            .await?;
        Ok(data_id)
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

    assert_eq!(
        context.read_data("wanted".to_string()).await.unwrap(),
        b"new"[..]
    );
}
