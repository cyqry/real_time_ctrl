use common::channel::Channel;
use ctrl_common::kik::Kik;
use log::error;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::{Mutex, RwLock};

#[derive(Clone, Debug)]
pub struct CtrlSession {
    pub session_id: String,
    pub ctrl_channel_id: String,
    pub created_at: SystemTime,
    used_data_nonces: HashSet<String>,
}

const CTRL_SESSION_TTL: std::time::Duration = std::time::Duration::from_secs(12 * 60 * 60);
const MAX_CTRL_DATA_CHANNELS: usize = 8;
const MAX_USED_DATA_NONCES: usize = 1024;

type SharedChannel = Arc<Mutex<Channel>>;
type CtrlConnections = Option<(Option<SharedChannel>, HashMap<String, SharedChannel>)>;

#[derive(Clone)]
//不要直接修改context,Context也可看为一个指针
pub struct Context {
    //todo 最外层优化为原子锁
    //当前控制者和它的数据连接
    ctrl_op: Arc<RwLock<CtrlConnections>>,
    next_ctrl_data: Arc<AtomicUsize>,
    ctrl_session: Arc<RwLock<Option<CtrlSession>>>,
    now_cmd_id: Arc<RwLock<Option<String>>>,
    //todo 最外层优化为原子锁
    //当前正在控制的kik
    //之所以要在Kik内部加一个arc，是因为会被kik_map变量共享引用，Kik的conn和vec都只能在堆中存在一份，kik_info是可克隆的，conn和vec锁分开是因为他们没有关系
    kik_op: Arc<RwLock<Option<Kik>>>,

    //所有在线的被控端map<String,Kik>
    pub kik_map: Arc<RwLock<HashMap<String, Kik>>>,
}

impl Context {
    pub fn init() -> Self {
        Context {
            ctrl_op: Arc::new(RwLock::new(None)),
            next_ctrl_data: Arc::new(AtomicUsize::new(0)),
            ctrl_session: Arc::new(RwLock::new(None)),
            now_cmd_id: Arc::new(RwLock::new(None)),
            kik_op: Arc::new(RwLock::new(None)),
            kik_map: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn find_ctrl_data(&self) -> Option<Arc<Mutex<Channel>>> {
        let next_arc = self.next_ctrl_data.clone();
        let ctrl_arc = self.ctrl_op.clone();
        let ctrl_guard = ctrl_arc.read().await;
        if ctrl_guard.is_some() {
            let data_map = ctrl_guard.clone().unwrap().1;
            if data_map.is_empty() {
                None
            } else {
                let next = next_arc.fetch_add(1, Ordering::Relaxed) % data_map.len();
                data_map.values().nth(next).cloned()
            }
        } else {
            None
        }
    }

    pub async fn now_cmd_id(&self) -> Option<String> {
        self.now_cmd_id.clone().read().await.clone()
    }
    pub async fn set_now_cmd_id_if_none(&self, id: String) -> bool {
        let arc = self.now_cmd_id.clone();
        let mut now_id = arc.write().await;
        match now_id.as_ref() {
            None => {
                *now_id = Some(id);
                true
            }
            Some(_) => false,
        }
    }

    pub async fn delete_now_cmd_id(&self) {
        *(self.now_cmd_id.clone().write().await) = None;
    }

    pub async fn exist_ctrl(&self) -> bool {
        self.ctrl_op
            .read()
            .await
            .as_ref()
            .and_then(|(channel, _)| channel.as_ref())
            .is_some()
    }

    pub async fn delete_ctrl_conn_if(&self, channel: &Arc<Mutex<Channel>>) -> bool {
        let data_connections = {
            let mut guard = self.ctrl_op.write().await;
            let is_current = guard
                .as_ref()
                .and_then(|(current, _)| current.as_ref())
                .is_some_and(|current| Arc::ptr_eq(current, channel));
            if !is_current {
                return false;
            }
            guard
                .take()
                .map(|(_, data)| data.into_values().collect::<Vec<_>>())
                .unwrap_or_default()
        };
        self.clear_ctrl_session().await;
        for data_connection in data_connections {
            data_connection.lock().await.try_write_half_close().await;
        }
        true
    }

    /// 替换控制连接时同时隔离旧数据通道；旧连接的 inactive 回调不能清掉新会话。
    pub async fn set_ctrl_conn(&self, channel: Arc<Mutex<Channel>>) {
        let (old_ctrl, old_data_connections) = {
            let mut guard = self.ctrl_op.write().await;
            let previous = guard.take();
            *guard = Some((Some(channel), HashMap::new()));
            match previous {
                Some((old_ctrl, old_data)) => {
                    (old_ctrl, old_data.into_values().collect::<Vec<_>>())
                }
                None => (None, Vec::new()),
            }
        };
        if let Some(old_ctrl) = old_ctrl {
            old_ctrl.lock().await.try_write_half_close().await;
        }
        for data_connection in old_data_connections {
            data_connection.lock().await.try_write_half_close().await;
        }
    }

    pub async fn get_ctrl_conn(&self) -> Option<Arc<Mutex<Channel>>> {
        let arc = self.ctrl_op.clone();
        let guard = arc.read().await;
        match *guard {
            None => None,
            Some((ref ctrl_conn, ref _data_conns)) => ctrl_conn.clone(),
        }
    }

    pub async fn delete_ctrl_data_conn(&self, data_conn: Arc<Mutex<Channel>>) {
        match *(self.ctrl_op.clone().write().await) {
            None => {}
            Some((ref _ctrl_conn, ref mut data_conns)) => {
                data_conns.remove(data_conn.lock().await.get_id());
            }
        };
    }

    pub async fn set_ctrl_session(&self, session_id: String, ctrl_channel_id: String) {
        let session = CtrlSession {
            session_id,
            ctrl_channel_id,
            created_at: SystemTime::now(),
            used_data_nonces: HashSet::new(),
        };
        *self.ctrl_session.write().await = Some(session);
    }

    pub async fn clear_ctrl_session(&self) {
        *self.ctrl_session.write().await = None;
    }

    pub async fn validate_ctrl_data_session(&self, session_id: &str, channel_nonce: &str) -> bool {
        let current_ctrl_id = match self.get_ctrl_conn().await {
            Some(channel) => channel.lock().await.get_id().to_string(),
            None => return false,
        };
        let mut guard = self.ctrl_session.write().await;
        let Some(session) = guard.as_mut() else {
            return false;
        };
        if session.session_id != session_id || session.ctrl_channel_id != current_ctrl_id {
            return false;
        }
        if session
            .created_at
            .elapsed()
            .map_or(true, |age| age > CTRL_SESSION_TTL)
        {
            *guard = None;
            return false;
        }
        if session.used_data_nonces.contains(channel_nonce) {
            return false;
        }
        if session.used_data_nonces.len() >= MAX_USED_DATA_NONCES {
            return false;
        }
        session.used_data_nonces.insert(channel_nonce.to_string());
        true
    }

    pub async fn insert_ctrl_data_conn(&self, data_conn: Arc<Mutex<Channel>>) -> bool {
        match *(self.ctrl_op.clone().write().await) {
            None => false,
            Some((ref ctrl_conn, ref mut data_conns)) => {
                if ctrl_conn.is_none() || data_conns.len() >= MAX_CTRL_DATA_CHANNELS {
                    return false;
                }
                data_conns.insert(
                    data_conn.clone().lock().await.get_id().to_string(),
                    data_conn,
                );
                true
            }
        }
    }

    //手动下线kik, 通过手动关闭连接自动触发下线
    pub async fn offline_kik(&self, kik_id: &str) {
        let offline_kik = self.find_kik(kik_id).await;
        if let Some(kik) = offline_kik {
            kik.clear().await;
        }
    }

    //清理对应id kik的 kik_conn,
    pub async fn delete_kik_conn_if_id(&self, id: &str) {
        //先从 kik_op找
        {
            let arc = self.kik_op.clone();
            let guard = arc.write().await;
            if let Some(kik) = guard.as_ref() {
                if kik.kik_client_info.kik_info.id.as_deref() == Some(id) {
                    kik.delete_kik_conn().await;
                    return;
                }
            }
        }
        //再去map中找
        let arc = self.kik_map.clone();
        let kik_map = arc.read().await;
        if let Some(kik) = kik_map.get(id) {
            kik.delete_kik_conn().await;
        }
    }

    pub async fn delete_kik_if_not_online(&self, kik_id: &str) -> Option<Kik> {
        //判断数据连接和kik还有没有，都没有就说明其彻底下线
        let kik = self.find_kik(kik_id).await?;
        if !kik.exist_data_channel().await && !kik.exist_kik_conn().await {
            //从整个context中删除这个kik
            self.just_delete_kik(kik_id).await
        } else {
            None
        }
    }

    async fn find_kik(&self, kik_id: &str) -> Option<Kik> {
        //先从 self.kik_op找
        let guard = self.kik_op.read().await;
        if let Some(ref kik) = *guard {
            if kik.kik_client_info.kik_info.id.as_deref() == Some(kik_id) {
                return Some(kik.clone());
            }
        }
        //map中的
        self.kik_map.read().await.get(kik_id).cloned()
    }

    async fn just_delete_kik(&self, kik_id: &str) -> Option<Kik> {
        //先从 self.kik_op找
        let kik_op = {
            let mut guard = self.kik_op.write().await;
            if let Some(ref kik) = *guard {
                if kik.kik_client_info.kik_info.id.as_deref() == Some(kik_id) {
                    guard.take()
                } else {
                    None
                }
            } else {
                None
            }
        };
        //删除map中的
        let map_kik = self.kik_map.write().await.remove(kik_id);
        if kik_op.is_some() {
            kik_op
        } else {
            map_kik
        }
    }

    pub async fn delete_kik_data_conn(&self, data_conn: Arc<Mutex<Channel>>) {
        //先从 kik_op中找
        {
            let kik_op = self.kik_op.clone().read().await.clone();
            if let Some(kik) = kik_op {
                kik.delete_data_conn(data_conn.clone()).await;
            }
        }
        //再从map中找
        //不要与下面写在一行，因为引用传递导致的生命周期问题或者match的一个生命周期问题，所不会getid了就释放，然后在match中 delete时又lock了所以死锁
        let kik_id = {
            let guard = data_conn.lock().await;
            let Some(kik_id) = guard.get::<String>("kik_id") else {
                return;
            };
            kik_id.to_string()
        };
        match self.kik_map.clone().read().await.get(kik_id.as_str()) {
            None => {}
            Some(kik) => {
                kik.delete_data_conn(data_conn).await;
            }
        };
    }

    pub async fn set_kik(&self, kik: Kik) {
        *(self.kik_op.clone().write().await) = Some(kik);
    }

    //当前正在控制的kik，一定是初始化完成的kik连接即initialized一定为true
    pub async fn get_kik(&self) -> Option<Kik> {
        let guard = self.kik_op.read().await;
        match *guard {
            None => {}
            Some(ref k) => {
                if !k.initialized() {
                    error!("取当前正在控制kik时，未初始化完成")
                }
            }
        }
        guard.clone()
    }

    pub async fn get_can_ctrl_kik(&self) -> Vec<(String, Kik)> {
        let snapshot = self
            .kik_map
            .read()
            .await
            .iter()
            .map(|(id, kik)| (id.clone(), kik.clone()))
            .collect::<Vec<_>>();
        let mut res = vec![];
        for (id, kik) in snapshot {
            if kik.initialized() && kik.exist_kik_conn().await {
                res.push((id, kik))
            }
        }
        res
    }

    pub async fn get_initialized_kik_by_id(&self, id: &str) -> Option<Kik> {
        let read = self.kik_map.read().await;
        let kik = read.get(id);

        kik.filter(|kik| kik.initialized()).cloned()
    }
}
