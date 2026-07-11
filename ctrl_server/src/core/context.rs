use crate::core::connection_meta::KIK_ID;
use common::channel::Channel;
use ctrl_common::kik::Kik;
use log::error;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::{Mutex, RwLock};

struct CtrlSession {
    session_id: String,
    created_at: SystemTime,
    used_data_nonces: HashSet<String>,
}

const CTRL_SESSION_TTL: std::time::Duration = std::time::Duration::from_secs(12 * 60 * 60);
const MAX_CTRL_DATA_CHANNELS: usize = 8;
const MAX_USED_DATA_NONCES: usize = 1024;

type SharedChannel = Arc<Mutex<Channel>>;

#[derive(Default)]
struct CtrlState {
    connection: Option<SharedChannel>,
    data_connections: HashMap<String, SharedChannel>,
    session: Option<CtrlSession>,
}

/// 服务端共享运行状态。克隆该类型只克隆内部状态句柄。
#[derive(Clone)]
pub struct Context {
    // 控制连接、数据连接与会话必须在同一个锁内切换，避免旧连接清理新会话。
    ctrl: Arc<RwLock<CtrlState>>,
    next_ctrl_data: Arc<AtomicUsize>,
    active_command_id: Arc<Mutex<Option<String>>>,
    selected_kik: Arc<RwLock<Option<Kik>>>,
    pub(crate) kiks: Arc<RwLock<HashMap<String, Kik>>>,
}

impl Context {
    pub fn init() -> Self {
        Context {
            ctrl: Arc::new(RwLock::new(CtrlState::default())),
            next_ctrl_data: Arc::new(AtomicUsize::new(0)),
            active_command_id: Arc::new(Mutex::new(None)),
            selected_kik: Arc::new(RwLock::new(None)),
            kiks: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn find_ctrl_data(&self) -> Option<Arc<Mutex<Channel>>> {
        let ctrl = self.ctrl.read().await;
        if ctrl.data_connections.is_empty() {
            return None;
        }
        let next =
            self.next_ctrl_data.fetch_add(1, Ordering::Relaxed) % ctrl.data_connections.len();
        ctrl.data_connections.values().nth(next).cloned()
    }

    pub async fn active_command_id(&self) -> Option<String> {
        self.active_command_id.lock().await.clone()
    }

    pub async fn try_begin_command(&self, id: String) -> bool {
        let mut now_id = self.active_command_id.lock().await;
        match now_id.as_ref() {
            None => {
                *now_id = Some(id);
                true
            }
            Some(_) => false,
        }
    }

    pub async fn finish_command(&self) {
        *self.active_command_id.lock().await = None;
    }

    pub async fn exist_ctrl(&self) -> bool {
        self.ctrl.read().await.connection.is_some()
    }

    pub async fn delete_ctrl_conn_if(&self, channel: &Arc<Mutex<Channel>>) -> bool {
        let data_connections: Vec<SharedChannel> = {
            let mut ctrl = self.ctrl.write().await;
            let is_current = ctrl
                .connection
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, channel));
            if !is_current {
                return false;
            }
            ctrl.connection = None;
            ctrl.session = None;
            ctrl.data_connections
                .drain()
                .map(|(_, data)| data)
                .collect()
        };
        for data_connection in data_connections {
            data_connection.lock().await.try_write_half_close().await;
        }
        true
    }

    /// 替换控制连接时同时隔离旧数据通道；旧连接的 inactive 回调不能清掉新会话。
    pub async fn set_ctrl_conn(&self, channel: Arc<Mutex<Channel>>) {
        self.replace_ctrl(channel, None).await;
    }

    pub async fn set_ctrl_conn_with_session(
        &self,
        channel: Arc<Mutex<Channel>>,
        session_id: String,
    ) {
        let session = CtrlSession {
            session_id,
            created_at: SystemTime::now(),
            used_data_nonces: HashSet::new(),
        };
        self.replace_ctrl(channel, Some(session)).await;
    }

    async fn replace_ctrl(&self, channel: Arc<Mutex<Channel>>, session: Option<CtrlSession>) {
        let (old_ctrl, old_data_connections) = {
            let mut ctrl = self.ctrl.write().await;
            let old_ctrl = ctrl.connection.replace(channel);
            let old_data = ctrl
                .data_connections
                .drain()
                .map(|(_, data)| data)
                .collect::<Vec<_>>();
            ctrl.session = session;
            (old_ctrl, old_data)
        };
        if let Some(old_ctrl) = old_ctrl {
            old_ctrl.lock().await.try_write_half_close().await;
        }
        for data_connection in old_data_connections {
            data_connection.lock().await.try_write_half_close().await;
        }
    }

    pub async fn delete_ctrl_data_conn(&self, data_conn: Arc<Mutex<Channel>>) {
        let id = data_conn.lock().await.id().map(str::to_owned);
        if let Some(id) = id {
            self.ctrl.write().await.data_connections.remove(&id);
        }
    }

    pub async fn validate_ctrl_data_session(&self, session_id: &str, channel_nonce: &str) -> bool {
        let mut ctrl = self.ctrl.write().await;
        if ctrl.connection.is_none() {
            return false;
        }
        let Some(session) = ctrl.session.as_mut() else {
            return false;
        };
        if session.session_id != session_id {
            return false;
        }
        if session
            .created_at
            .elapsed()
            .map_or(true, |age| age > CTRL_SESSION_TTL)
        {
            ctrl.session = None;
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
        let Some(id) = data_conn.lock().await.id().map(str::to_owned) else {
            return false;
        };
        let mut ctrl = self.ctrl.write().await;
        if ctrl.connection.is_none() || ctrl.data_connections.len() >= MAX_CTRL_DATA_CHANNELS {
            return false;
        }
        ctrl.data_connections.insert(id, data_conn);
        true
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
        // 先复制句柄再释放外层锁，不能在等待内部连接锁时阻塞当前选择状态。
        let selected = self
            .selected_kik
            .read()
            .await
            .clone()
            .filter(|kik| kik.kik_client_info.kik_info.id.as_deref() == Some(id));
        if let Some(kik) = selected {
            kik.delete_kik_conn().await;
            return;
        }
        let mapped = self.kiks.read().await.get(id).cloned();
        if let Some(kik) = mapped {
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
        let guard = self.selected_kik.read().await;
        if let Some(ref kik) = *guard {
            if kik.kik_client_info.kik_info.id.as_deref() == Some(kik_id) {
                return Some(kik.clone());
            }
        }
        //map中的
        self.kiks.read().await.get(kik_id).cloned()
    }

    async fn just_delete_kik(&self, kik_id: &str) -> Option<Kik> {
        let selected_kik = {
            let mut guard = self.selected_kik.write().await;
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
        let mapped_kik = self.kiks.write().await.remove(kik_id);
        if selected_kik.is_some() {
            selected_kik
        } else {
            mapped_kik
        }
    }

    pub async fn delete_kik_data_conn(&self, data_conn: Arc<Mutex<Channel>>) {
        // 先从当前选中项清理，再清理在线表中的同一共享会话。
        {
            let selected_kik = self.selected_kik.read().await.clone();
            if let Some(kik) = selected_kik {
                kik.delete_data_conn(data_conn.clone()).await;
            }
        }
        //再从map中找
        //不要与下面写在一行，因为引用传递导致的生命周期问题或者match的一个生命周期问题，所不会getid了就释放，然后在match中 delete时又lock了所以死锁
        let kik_id = {
            let guard = data_conn.lock().await;
            let Some(kik_id) = guard.attribute(&KIK_ID) else {
                return;
            };
            kik_id.to_string()
        };
        let mapped = self.kiks.read().await.get(kik_id.as_str()).cloned();
        if let Some(kik) = mapped {
            kik.delete_data_conn(data_conn).await;
        }
    }

    pub async fn set_kik(&self, kik: Kik) {
        *self.selected_kik.write().await = Some(kik);
    }

    //当前正在控制的kik，一定是初始化完成的kik连接即initialized一定为true
    pub async fn get_kik(&self) -> Option<Kik> {
        let kik = self.selected_kik.read().await.clone()?;
        if !kik.initialized() {
            error!("取当前正在控制 Kik 时连接尚未初始化完成");
            return None;
        }
        Some(kik)
    }

    pub async fn get_can_ctrl_kik(&self) -> Vec<(String, Kik)> {
        let snapshot = self
            .kiks
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
        let read = self.kiks.read().await;
        let kik = read.get(id);

        kik.filter(|kik| kik.initialized()).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::channel::ChannelType;
    use std::io;

    fn channel(id: &str, channel_type: ChannelType) -> SharedChannel {
        let (_peer, stream) = tokio::io::duplex(64);
        Arc::new(Mutex::new(Channel::new(
            Box::pin(stream),
            Some(id.to_string()),
            channel_type,
            Err(io::Error::new(io::ErrorKind::NotConnected, "test")),
            Err(io::Error::new(io::ErrorKind::NotConnected, "test")),
        )))
    }

    #[tokio::test]
    async fn stale_ctrl_cleanup_cannot_remove_new_session() {
        let context = Context::init();
        let old = channel("old", ChannelType::Ctrl);
        let new = channel("new", ChannelType::Ctrl);

        context
            .set_ctrl_conn_with_session(old.clone(), "old-session".to_string())
            .await;
        context
            .set_ctrl_conn_with_session(new, "new-session".to_string())
            .await;

        assert!(!context.delete_ctrl_conn_if(&old).await);
        assert!(
            context
                .validate_ctrl_data_session("new-session", "new-nonce")
                .await
        );
        assert!(
            !context
                .validate_ctrl_data_session("old-session", "old-nonce")
                .await
        );
    }

    #[tokio::test]
    async fn current_ctrl_cleanup_removes_session_and_data_channels() {
        let context = Context::init();
        let ctrl = channel("ctrl", ChannelType::Ctrl);
        let data = channel("data", ChannelType::CtrlData);

        context
            .set_ctrl_conn_with_session(ctrl.clone(), "session".to_string())
            .await;
        assert!(context.insert_ctrl_data_conn(data).await);
        assert!(context.delete_ctrl_conn_if(&ctrl).await);

        assert!(context.find_ctrl_data().await.is_none());
        assert!(!context.validate_ctrl_data_session("session", "nonce").await);
    }
}
