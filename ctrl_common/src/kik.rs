use crate::entity::KikClientInfo;
use common::channel::Channel;
use common::kik_info::KikInfo;
use common::message::kik_resp::KikResp;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::SystemTime;
use tokio::sync::{Mutex, OwnedSemaphorePermit, RwLock, Semaphore, TryAcquireError, oneshot};

/// 服务端持有的单个被控端会话。
///
/// 克隆 `Kik` 只克隆共享状态句柄，不会复制底层连接。
#[derive(Clone)]
pub struct Kik {
    // Kik 创建完成后 id 一定存在；None 仅用于首次注册请求。
    pub kik_client_info: KikClientInfo,
    conn_op: Arc<RwLock<Option<Arc<Mutex<Channel>>>>>,
    //data conn 的getid是 random id,  attr 一个 kik id;这里的key为 data conn的get_id
    data_conns: Arc<Mutex<HashMap<String, Arc<Mutex<Channel>>>>>,
    next_data_conn: Arc<AtomicUsize>,

    //是否已上线(只在初始化时修改一次)
    initialized: Arc<AtomicBool>,
    pending_commands: Arc<Mutex<HashMap<String, oneshot::Sender<KikResp>>>>,
    command_limit: Arc<Semaphore>,
}

impl Kik {
    pub fn new(
        id: &str,
        name: &str,
        ip: String,
        recent_online_time: SystemTime,
        conn: Arc<Mutex<Channel>>,
    ) -> Self {
        Kik {
            kik_client_info: KikClientInfo {
                kik_info: KikInfo {
                    id: Some(id.to_string()),
                    name: name.to_string(),
                },
                ip: Arc::new(RwLock::new(ip)),
                recent_online_time: Arc::new(RwLock::new(recent_online_time)),
            },
            next_data_conn: Arc::new(AtomicUsize::new(0)),
            conn_op: Arc::new(RwLock::new(Some(conn))),
            data_conns: Arc::new(Mutex::new(HashMap::new())),
            initialized: Arc::new(AtomicBool::new(false)),
            pending_commands: Arc::new(Mutex::new(HashMap::new())),
            // 同一被控端允许有限并行，防止一个慢 Kik 被大量已认证请求耗尽服务端内存。
            command_limit: Arc::new(Semaphore::new(16)),
        }
    }

    pub fn id(&self) -> Option<&str> {
        self.kik_client_info.kik_info.id.as_deref()
    }

    pub async fn find_data_conn(&self) -> Option<Arc<Mutex<Channel>>> {
        self.data_connections_for_send().await.into_iter().next()
    }

    /// 返回以轮询游标开头的连接快照，供单帧失败后尝试其他连接。
    pub async fn data_connections_for_send(&self) -> Vec<Arc<Mutex<Channel>>> {
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

    pub fn set_kik_initialized(&self, initialized: bool) {
        self.initialized.store(initialized, Ordering::Release);
    }

    pub fn initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    pub async fn exist_kik_conn(&self) -> bool {
        self.conn_op.read().await.is_some()
    }

    pub async fn get_kik_conn(&self) -> Option<Arc<Mutex<Channel>>> {
        self.conn_op.read().await.clone()
    }
    pub async fn is_kik_conn(&self, channel: &Arc<Mutex<Channel>>) -> bool {
        self.conn_op
            .read()
            .await
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, channel))
    }
    pub async fn delete_kik_conn(&self) -> Option<Arc<Mutex<Channel>>> {
        self.conn_op.write().await.take()
    }
    /// 只清理由该回调持有的连接，防止旧连接迟到的 inactive 事件删除刚完成的重连。
    pub async fn delete_kik_conn_if(&self, channel: &Arc<Mutex<Channel>>) -> bool {
        let removed = {
            let mut current = self.conn_op.write().await;
            if current
                .as_ref()
                .is_some_and(|registered| Arc::ptr_eq(registered, channel))
            {
                current.take();
                true
            } else {
                false
            }
        };
        if removed {
            self.pending_commands.lock().await.clear();
        }
        removed
    }
    pub async fn set_kik_conn(&self, conn: Arc<Mutex<Channel>>) -> Option<Arc<Mutex<Channel>>> {
        // 重连不会重放旧命令，立即取消旧连接上的等待者，避免占用许可直到超时。
        self.pending_commands.lock().await.clear();
        self.conn_op.write().await.replace(conn)
    }

    pub async fn delete_data_conn(&self, conn: Arc<Mutex<Channel>>) -> Option<Arc<Mutex<Channel>>> {
        let id = conn.lock().await.id().map(str::to_owned)?;
        self.data_conns.lock().await.remove(id.as_str())
    }

    pub async fn insert_data_conn(&self, conn: Arc<Mutex<Channel>>) -> bool {
        let Some(id) = conn.lock().await.id().map(str::to_owned) else {
            return false;
        };
        let mut connections = self.data_conns.lock().await;
        if connections.len() >= 4 && !connections.contains_key(&id) {
            return false;
        }
        connections.insert(id, conn);
        true
    }
    pub async fn exist_data_channel(&self) -> bool {
        !self.data_conns.lock().await.is_empty()
    }
    pub async fn clear(&self) {
        self.pending_commands.lock().await.clear();
        // 先移出连接再等待网络关闭，避免一个慢连接长期占住会话状态锁。
        let data_connections = self
            .data_conns
            .lock()
            .await
            .drain()
            .map(|(_, connection)| connection)
            .collect::<Vec<_>>();
        for connection in data_connections {
            connection.lock().await.try_write_half_close().await;
        }
        if let Some(connection) = self.conn_op.write().await.take() {
            connection.lock().await.try_write_half_close().await;
        }
    }

    pub fn try_acquire_command(&self) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        self.command_limit.clone().try_acquire_owned()
    }

    pub async fn register_command(
        &self,
        command_id: String,
    ) -> anyhow::Result<oneshot::Receiver<KikResp>> {
        use std::collections::hash_map::Entry;
        let mut pending = self.pending_commands.lock().await;
        match pending.entry(command_id) {
            Entry::Vacant(entry) => {
                let (tx, rx) = oneshot::channel();
                entry.insert(tx);
                Ok(rx)
            }
            Entry::Occupied(_) => Err(anyhow::anyhow!("被控端命令关联 ID 冲突")),
        }
    }

    pub async fn cancel_command(&self, command_id: &str) {
        self.pending_commands.lock().await.remove(command_id);
    }

    /// 响应只唤醒拥有该内部关联 ID 的请求；未知或超时后的响应直接丢弃。
    pub async fn complete_command(&self, command_id: &str, response: KikResp) -> bool {
        let sender = self.pending_commands.lock().await.remove(command_id);
        sender.is_some_and(|sender| sender.send(response).is_ok())
    }
}
