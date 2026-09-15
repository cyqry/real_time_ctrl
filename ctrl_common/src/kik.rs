//! 服务端视角的单个在线 Kik 会话。
//!
//! 一个 Kik 有一条主连接、多条数据连接、一个有界命令许可池和按内部命令 ID 索引的 oneshot 等待者。
//! 结构体可克隆，但克隆只复制 `Arc`，所有任务仍观察同一份连接和路由状态。

use crate::entity::KikClientInfo;
use common::channel::Channel;
use common::kik_info::KikInfo;
use common::message::kik_resp::KikResp;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{
    Mutex, Notify, OwnedSemaphorePermit, RwLock, Semaphore, TryAcquireError, oneshot,
};

/// 服务端持有的单个被控端会话。
///
/// 克隆 `Kik` 只克隆共享状态句柄，不会复制底层连接。
#[derive(Clone)]
pub struct Kik {
    /// Kik 创建完成后 ID 一定存在；`None` 只会出现在尚未注册的 `KikInfo` 输入中。
    pub kik_client_info: KikClientInfo,
    /// 当前主连接。重连会原子替换它，旧连接的清理回调必须用指针身份确认后再删除。
    conn_op: Arc<RwLock<Option<Arc<Mutex<Channel>>>>>,
    /// 数据连接以连接自身的随机 ID 为键；每条连接属性中另外保存所属 Kik ID。
    data_conns: Arc<Mutex<HashMap<String, Arc<Mutex<Channel>>>>>,
    /// 新数据连接入池时唤醒正在等待链路恢复的文件转发任务。
    ///
    /// `Notify` 不携带业务数据，仅表示“连接集合可能发生了变化”；等待者醒来后必须重新检查连接池。
    data_connection_notify: Arc<Notify>,
    /// 只用于负载轮询，不参与安全判断，因此使用 Relaxed 原子序即可。
    next_data_conn: Arc<AtomicUsize>,

    /// 初始化完成标志；只有置为 true 后才允许出现在控制端的在线列表中。
    initialized: Arc<AtomicBool>,
    /// 服务端内部命令 ID 到独立响应发送端的映射，防止并发响应串单。
    pending_commands: Arc<Mutex<HashMap<String, oneshot::Sender<KikResp>>>>,
    /// 单个 Kik 的命令并发上限，许可随命令任务的 RAII 生命周期释放。
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
            data_connection_notify: Arc::new(Notify::new()),
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
        drop(connections);
        // 同一个 Kik 可能承载多个并行文件任务；连接池恢复时全部唤醒，让它们重新竞争健康连接。
        self.data_connection_notify.notify_waiters();
        true
    }

    /// 等待至少一条未被标记为关闭的数据连接，并返回轮询后的连接快照。
    ///
    /// 文件分片已经从源连接完整读入后，目标连接可能正处于自动补建窗口。短暂等待比立即丢弃该帧
    /// 更可靠；总等待时间由调用方限制，因此失联客户端不会永久占住服务端任务。
    pub async fn wait_data_connections_for_send(
        &self,
        wait_timeout: Duration,
    ) -> Vec<Arc<Mutex<Channel>>> {
        let deadline = Instant::now() + wait_timeout;
        loop {
            let notified = self.data_connection_notify.notified();
            tokio::pin!(notified);
            // `enable` 会在检查连接池前完成等待者登记，因此广播不会丢在检查与 await 的缝隙里。
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
            if remaining.is_zero()
                || tokio::time::timeout(remaining, notified.as_mut())
                    .await
                    .is_err()
            {
                return Vec::new();
            }
        }
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
