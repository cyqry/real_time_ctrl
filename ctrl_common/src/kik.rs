use crate::entity::KikClientInfo;
use chrono::{DateTime, Local};
use common::channel::Channel;
use common::kik_info::KikInfo;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::{Mutex, RwLock};

/// 服务端持有的单个被控端会话。
///
/// 克隆 `Kik` 只克隆共享状态句柄，不会复制底层连接。
#[derive(Clone)]
pub struct Kik {
    // Kik 创建完成后 id 一定存在；协议层仍保留 Option 以兼容首次注册请求。
    pub kik_client_info: KikClientInfo,
    conn_op: Arc<RwLock<Option<Arc<Mutex<Channel>>>>>,
    //data conn 的getid是 random id,  attr 一个 kik id;这里的key为 data conn的get_id
    data_conns: Arc<Mutex<HashMap<String, Arc<Mutex<Channel>>>>>,
    next_data_conn: Arc<AtomicUsize>,

    //是否已上线(只在初始化时修改一次)
    initialized: Arc<AtomicBool>,
}

#[derive(Clone)]
pub struct KikLifeTime {
    //Kik中的kik_info的id一定是Some的
    pub kik_info: KikInfo,

    pub online_time: DateTime<Local>,
    pub offline_time: DateTime<Local>,
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
        }
    }

    pub fn id(&self) -> Option<&str> {
        self.kik_client_info.kik_info.id.as_deref()
    }

    pub async fn find_data_conn(&self) -> Option<Arc<Mutex<Channel>>> {
        let data_map = self.data_conns.lock().await;
        if data_map.is_empty() {
            None
        } else {
            let next = self.next_data_conn.fetch_add(1, Ordering::Relaxed) % data_map.len();
            data_map.values().nth(next).cloned()
        }
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
    pub async fn delete_kik_conn(&self) -> Option<Arc<Mutex<Channel>>> {
        self.conn_op.write().await.take()
    }
    pub async fn set_kik_conn(&self, conn: Arc<Mutex<Channel>>) -> Option<Arc<Mutex<Channel>>> {
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
}
