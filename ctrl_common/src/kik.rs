use common::channel::Channel;
use common::kik_info::KikInfo;
use chrono::{DateTime, Local};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU16, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime};
use tokio::io::AsyncReadExt;
use tokio::sync::{Mutex, RwLock};
use crate::entity::KikClientInfo;

//Kik可看做指向一个(conn，data_conns)的指针
#[derive(Clone)]
pub struct Kik {
    //Kik中的kik_info的id一定是Some的
    pub kik_client_info: KikClientInfo,
    // conn的id直接是 kik_id
    //todo 优化为原子锁
    conn_op: Arc<RwLock<Option<Arc<Mutex<Channel>>>>>,
    //data conn 的getid是 random id,  attr 一个 kik id;这里的key为 data conn的get_id
    data_conns: Arc<Mutex<HashMap<String, Arc<Mutex<Channel>>>>>,
    next_data_conn: Arc<AtomicU16>,
    
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
    pub fn new(id: &str, name: &str, ip: String, recent_online_time: SystemTime, conn: Arc<Mutex<Channel>>) -> Self {
        Kik {
            kik_client_info:KikClientInfo{
                kik_info: KikInfo {
                    id: Some(id.to_string()),
                    name: name.to_string(),
                },
                ip: Arc::new(RwLock::new(ip)),
                recent_online_time: Arc::new(RwLock::new(recent_online_time)),
            },
            next_data_conn: Arc::new(AtomicU16::new(0)),
            conn_op: Arc::new(RwLock::new(Some(conn))),
            data_conns: Arc::new(Mutex::new(HashMap::new())),
            initialized: Arc::new(AtomicBool::new(false)),
        }
    }
    pub async fn find_data_conn(&self) -> Option<Arc<Mutex<Channel>>> {
        let next_arc = self.next_data_conn.clone();
        let data_map_arc = self.data_conns.clone();
        let data_map = data_map_arc.lock().await;
        if data_map.len() == 0 {
            None
        } else {
            let next = (next_arc.load(Ordering::SeqCst) + 1) % data_map.len() as u16;
            let c = data_map.values().nth(next as usize).unwrap().clone();
            next_arc.store(next + 1, Ordering::SeqCst);
            Some(c)
        }
    }

    pub fn set_kik_initialized(&self, initialized: bool) {
        self.initialized.store(initialized, Ordering::SeqCst);
    }
    
    pub fn initialized(&self) -> bool {
        self.initialized.load(Ordering::SeqCst)
    }
    
    pub async fn exist_kik_conn(&self) -> bool {
        self.conn_op.clone().read().await.is_some()
    }

    pub async fn get_kik_conn(&self) -> Option<Arc<Mutex<Channel>>> {
        self.conn_op.clone().read().await.clone()
    }
    pub async fn delete_kik_conn(&self) -> Option<Arc<Mutex<Channel>>> {
        let arc = self.conn_op.clone();
        let mut guard = arc.write().await;
        let option = guard.clone();
        *guard = None;
        option
    }
    pub async fn set_kik_conn(&self, conn: Arc<Mutex<Channel>>) -> Option<Arc<Mutex<Channel>>> {
        let arc = self.conn_op.clone();
        let mut guard = arc.write().await;
        let option = guard.clone();
        *(guard) = Some(conn);
        option
    }

    pub async fn delete_data_conn(&self, conn: Arc<Mutex<Channel>>) -> Option<Arc<Mutex<Channel>>> {
        let id = conn.lock().await.get_id().to_string();
        self.data_conns.lock().await.remove(id.as_str())
    }

    pub async fn insert_data_conn(&self, conn: Arc<Mutex<Channel>>) {
        self.data_conns
            .lock()
            .await
            .insert(conn.clone().lock().await.get_id().to_string(), conn);
    }
    pub async fn exist_data_channel(&self) -> bool {
        !self.data_conns.clone().lock().await.is_empty()
    }
    pub async fn clear(&self) {
        {
            //data_conn的清理
            let arc = self.data_conns.clone();
            let mut guard = arc.lock().await;
            for x in guard.values() {
                x.lock().await.try_write_half_close().await;
            }
            guard.clear();
        }
        //kik_conn 关闭
        match self.conn_op.write().await.clone() {
            None => {}
            Some(conn) => {
                conn.lock().await.try_write_half_close().await;
            }
        };
    }
}
