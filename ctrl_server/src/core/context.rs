//! 服务端跨连接共享的会话、Kik、数据路由和并发配额。
//!
//! 这是服务端状态机的中心：Ctrl 主连接创建会话，CtrlData 绑定会话，Kik/KikData 登记被控端，
//! 命令执行临时建立数据 ID 映射。集合锁内只做内存操作，网络关闭与写入必须在锁外等待。

use crate::core::account::{AccountPolicy, AccountRegistry};
use crate::core::connection_meta::{CTRL_SESSION_ID, KIK_ID};
use common::channel::Channel;
use common::command::{Command, CtrlCommand};
use ctrl_common::cmd_resp_info::KikPresenceVo;
use ctrl_common::kik::Kik;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, RwLock, Semaphore};
use uuid::Uuid;

const CTRL_SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const DATA_ROUTE_TTL: Duration = Duration::from_secs(4 * 60 * 60 + 15 * 60);
const MAX_CTRL_DATA_CHANNELS: usize = 8;
const MAX_USED_DATA_NONCES: usize = 1024;
const MAX_DATA_ROUTES: usize = 4096;
const MAX_SESSION_DATA_ROUTES: usize = 64;
const MAX_GLOBAL_COMMANDS: usize = 256;
const MAX_EARLY_CTRL_DATA_FRAMES: usize = 32;
const EARLY_CTRL_DATA_WAIT: Duration = Duration::from_secs(5);
const MAX_KIK_PRESENCE_RECORDS: usize = 256;

/// 网络任务共享一个写半连接时的标准所有权形式。
type SharedChannel = Arc<Mutex<Channel>>;

/// 一条已通过 HMAC 挑战的控制主会话。
///
/// `selected_kik_id` 是会话私有选择；`data_connections`、nonce 集合和实例级命令许可也不能跨会话复用。
struct CtrlSession {
    /// 决定认证 secret、Kik ACL 和账号级并发上限。
    account_id: String,
    /// 区分同一账号的多个控制进程；只有同账号同实例的重连会替换旧会话。
    instance_id: String,
    /// Ctrl 主连接，负责命令和响应。
    connection: SharedChannel,
    /// 已用 session proof 绑定的 CtrlData 连接，键为连接随机 ID。
    data_connections: HashMap<String, SharedChannel>,
    /// 选择下一条数据连接的轮询游标。
    next_data_connection: Arc<AtomicUsize>,
    /// 当前命令默认发往的 Kik；为空时由自动选择逻辑补充。
    selected_kik_id: Option<String>,
    /// 用于限制会话最长寿命，避免永久 session。
    created_at: SystemTime,
    /// 已消费的数据通道 nonce，防止同一 proof 被重放建立更多连接。
    used_data_nonces: HashSet<String>,
    /// 当前实例独享的命令许可池。
    command_limit: Arc<Semaphore>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
/// 数据路由方向也是授权条件，防止同一个字符串 ID 被反向利用。
pub enum DataDirection {
    CtrlToKik,
    KikToCtrl,
}

#[derive(Clone)]
/// 一条控制数据 ID 到服务端内部 wire ID 的临时绑定。
struct DataRoute {
    /// 路由所属控制会话，是跨账号隔离的第一层键。
    session_id: String,
    /// 本次命令锁定的 Kik，后续切换当前目标不会改变在途文件去向。
    kik_id: String,
    /// 服务端发给 Kik 的随机 ID，不直接暴露控制端提供的 ID。
    wire_data_id: String,
    /// 相同字符串只能按登记方向使用。
    direction: DataDirection,
    /// 小文件/截图只有一帧，转发完成即可自动回收；大文件等待显式完成或超时。
    single_frame: bool,
    /// 最终兜底清理时间，防止断线异常留下永久路由。
    expires_at: Instant,
}

/// 同一账号的多个实例共享 ACL，但会话选择、命令许可和数据通道完全隔离。
///
/// `Context` 的克隆成本很低，只复制共享状态句柄，适合移动进每个连接和命令任务。
#[derive(Clone)]
pub struct Context {
    accounts: AccountRegistry,
    sessions: Arc<RwLock<HashMap<String, Arc<Mutex<CtrlSession>>>>>,
    instances: Arc<Mutex<HashMap<(String, String), String>>>,
    data_routes: Arc<Mutex<HashMap<String, DataRoute>>>,
    data_route_notify: Arc<Notify>,
    early_ctrl_data_limit: Arc<Semaphore>,
    global_command_limit: Arc<Semaphore>,
    pub(crate) kiks: Arc<RwLock<HashMap<String, Kik>>>,
    kik_presence: Arc<RwLock<HashMap<String, KikPresenceVo>>>,
}

impl Context {
    #[cfg(test)]
    pub fn init() -> Self {
        let accounts = AccountRegistry::from_json_or_default(
            None,
            "development-control-secret-0000000000000000".to_string(),
        )
        .expect("内置测试账号配置必须有效");
        Self::init_with_accounts(accounts)
    }

    pub fn init_with_accounts(accounts: AccountRegistry) -> Self {
        Self {
            accounts,
            sessions: Arc::new(RwLock::new(HashMap::new())),
            instances: Arc::new(Mutex::new(HashMap::new())),
            data_routes: Arc::new(Mutex::new(HashMap::new())),
            data_route_notify: Arc::new(Notify::new()),
            early_ctrl_data_limit: Arc::new(Semaphore::new(MAX_EARLY_CTRL_DATA_FRAMES)),
            global_command_limit: Arc::new(Semaphore::new(MAX_GLOBAL_COMMANDS)),
            kiks: Arc::new(RwLock::new(HashMap::new())),
            kik_presence: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn account(&self, account_id: &str) -> Option<AccountPolicy> {
        self.accounts.get(account_id)
    }

    /// 原子注册控制会话，并按 `(account_id, instance_id)` 替换同实例旧会话。
    ///
    /// 新实例先检查账号实例配额；替换完成后在锁外关闭旧连接，避免网络 I/O 阻塞全局索引。
    pub async fn register_ctrl_session(
        &self,
        channel: SharedChannel,
        session_id: String,
        account_id: String,
        instance_id: String,
    ) -> anyhow::Result<()> {
        let policy = self
            .account(&account_id)
            .ok_or_else(|| anyhow::anyhow!("控制连接校验失败"))?;
        let instance_key = (account_id.clone(), instance_id.clone());
        let session = Arc::new(Mutex::new(CtrlSession {
            account_id,
            instance_id,
            connection: channel.clone(),
            data_connections: HashMap::new(),
            next_data_connection: Arc::new(AtomicUsize::new(0)),
            selected_kik_id: None,
            created_at: SystemTime::now(),
            used_data_nonces: HashSet::new(),
            command_limit: Arc::new(Semaphore::new(policy.max_commands_per_instance)),
        }));
        channel
            .lock()
            .await
            .insert_attribute(&CTRL_SESSION_ID, session_id.clone());

        // 实例索引和会话表必须在同一临界区提交。否则两个同实例认证并发完成时，
        // 较新的连接可能找不到尚未写入的旧 session，留下无法由实例索引清理的孤立会话。
        let old_session_id = {
            let mut instances = self.instances.lock().await;
            let active_for_account = instances
                .keys()
                .filter(|(account, _)| account == &instance_key.0)
                .count();
            let replacing = instances.get(&instance_key).cloned();
            if replacing.is_none() && active_for_account >= policy.max_instances {
                anyhow::bail!("账号活动控制实例达到上限");
            }
            let mut sessions = self.sessions.write().await;
            let old_session_id = instances.insert(instance_key, session_id.clone());
            sessions.insert(session_id.clone(), session);
            old_session_id
        };

        if let Some(old_session_id) = old_session_id {
            self.remove_session(&old_session_id, None).await;
        }
        // 控制端和 Kik 的连接顺序没有保证。会话注册完成后再做一次自动选择，
        // 与 Kik 上线侧的对应逻辑共同消除“双方恰好同时上线”时的漏选窗口。
        self.ensure_session_has_selected_kik(&session_id).await;
        Ok(())
    }

    pub async fn session_auth_secret(&self, session_id: &str) -> Option<Arc<str>> {
        let session = self.sessions.read().await.get(session_id).cloned()?;
        let account_id = session.lock().await.account_id.clone();
        self.account(&account_id).map(|policy| policy.secret)
    }

    /// 消费一次 CtrlData nonce。返回 false 表示会话过期、nonce 重放或记录已达上限。
    pub async fn validate_ctrl_data_session(&self, session_id: &str, channel_nonce: &str) -> bool {
        let Some(session) = self.sessions.read().await.get(session_id).cloned() else {
            return false;
        };
        let mut session = session.lock().await;
        if session
            .created_at
            .elapsed()
            .map_or(true, |age| age > CTRL_SESSION_TTL)
            || session.used_data_nonces.contains(channel_nonce)
            || session.used_data_nonces.len() >= MAX_USED_DATA_NONCES
        {
            return false;
        }
        session.used_data_nonces.insert(channel_nonce.to_string());
        true
    }

    pub async fn insert_ctrl_data_conn(&self, session_id: &str, data_conn: SharedChannel) -> bool {
        let Some(id) = data_conn.lock().await.id().map(str::to_owned) else {
            return false;
        };
        let Some(session) = self.sessions.read().await.get(session_id).cloned() else {
            return false;
        };
        let mut session = session.lock().await;
        if session.data_connections.len() >= MAX_CTRL_DATA_CHANNELS {
            return false;
        }
        session.data_connections.insert(id, data_conn.clone());
        data_conn
            .lock()
            .await
            .insert_attribute(&CTRL_SESSION_ID, session_id.to_string());
        true
    }

    pub async fn delete_ctrl_conn_if(&self, channel: &SharedChannel) -> bool {
        let session_id = channel.lock().await.attribute(&CTRL_SESSION_ID).cloned();
        let Some(session_id) = session_id else {
            return false;
        };
        self.remove_session(&session_id, Some(channel)).await
    }

    async fn remove_session(&self, session_id: &str, expected: Option<&SharedChannel>) -> bool {
        let session = self.sessions.read().await.get(session_id).cloned();
        let Some(session) = session else { return false };
        if let Some(expected) = expected {
            let session_guard = session.lock().await;
            if !Arc::ptr_eq(&session_guard.connection, expected) {
                return false;
            }
        }
        let session = self.sessions.write().await.remove(session_id);
        let Some(session) = session else { return false };
        let (key, control_connection, data_connections) = {
            let mut session = session.lock().await;
            (
                (session.account_id.clone(), session.instance_id.clone()),
                session.connection.clone(),
                session
                    .data_connections
                    .drain()
                    .map(|(_, channel)| channel)
                    .collect::<Vec<_>>(),
            )
        };
        let mut instances = self.instances.lock().await;
        if instances.get(&key).is_some_and(|id| id == session_id) {
            instances.remove(&key);
        }
        drop(instances);
        self.data_routes
            .lock()
            .await
            .retain(|_, route| route.session_id != session_id);
        control_connection.lock().await.try_write_half_close().await;
        for connection in data_connections {
            connection.lock().await.try_write_half_close().await;
        }
        true
    }

    pub async fn delete_ctrl_data_conn(&self, data_conn: SharedChannel) {
        let (session_id, id) = {
            let guard = data_conn.lock().await;
            (
                guard.attribute(&CTRL_SESSION_ID).cloned(),
                guard.id().map(str::to_owned),
            )
        };
        let (Some(session_id), Some(id)) = (session_id, id) else {
            return;
        };
        let session = self.sessions.read().await.get(&session_id).cloned();
        if let Some(session) = session {
            session.lock().await.data_connections.remove(&id);
        }
    }

    pub async fn ctrl_data_connections_for_send(&self, session_id: &str) -> Vec<SharedChannel> {
        let Some(session) = self.sessions.read().await.get(session_id).cloned() else {
            return Vec::new();
        };
        let session = session.lock().await;
        let count = session.data_connections.len();
        if count == 0 {
            return Vec::new();
        }
        let start = session.next_data_connection.fetch_add(1, Ordering::Relaxed) % count;
        session
            .data_connections
            .values()
            .cycle()
            .skip(start)
            .take(count)
            .cloned()
            .collect()
    }

    /// 同时取得全局、账号和实例三级许可；Kik 级许可在选定目标后取得。
    ///
    /// 返回的三个 RAII permit 必须由命令任务持有到响应和必要的数据处理结束。
    pub async fn try_acquire_command(
        &self,
        session_id: &str,
    ) -> anyhow::Result<(
        OwnedSemaphorePermit,
        OwnedSemaphorePermit,
        OwnedSemaphorePermit,
    )> {
        let global = self
            .global_command_limit
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow::anyhow!("服务端命令并发达到上限"))?;
        let session = self
            .sessions
            .read()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("控制会话已失效"))?;
        let (account_id, instance_limit) = {
            let session = session.lock().await;
            (session.account_id.clone(), session.command_limit.clone())
        };
        let account = self
            .account(&account_id)
            .ok_or_else(|| anyhow::anyhow!("控制账号已失效"))?
            .command_limit
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow::anyhow!("当前账号命令并发达到上限"))?;
        let local = instance_limit
            .try_acquire_owned()
            .map_err(|_| anyhow::anyhow!("当前控制实例命令并发达到上限"))?;
        Ok((global, account, local))
    }

    pub async fn set_kik(&self, session_id: &str, kik_id: &str) -> anyhow::Result<Kik> {
        let policy = self.policy_for_session(session_id).await?;
        if !policy.allows_kik(kik_id) {
            anyhow::bail!("当前账号无权访问该 Kik");
        }
        let kik = self
            .get_initialized_kik_by_id(kik_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("找不到该 Kik"))?;
        let session = self
            .sessions
            .read()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("控制会话已失效"))?;
        session.lock().await.selected_kik_id = Some(kik_id.to_string());
        Ok(kik)
    }

    pub async fn get_kik(&self, session_id: &str) -> Option<Kik> {
        let session = self.sessions.read().await.get(session_id).cloned()?;
        let kik_id = session.lock().await.selected_kik_id.clone()?;
        self.get_initialized_kik_by_id(&kik_id).await
    }

    /// 为尚未选择目标的控制会话选择最近上线且有权限访问的 Kik。
    ///
    /// 候选扫描和连接状态读取均在集合锁外完成，避免慢连接状态锁阻塞全局会话注册。
    async fn ensure_session_has_selected_kik(&self, session_id: &str) {
        let Some(session) = self.sessions.read().await.get(session_id).cloned() else {
            return;
        };
        let account_id = {
            let session = session.lock().await;
            if session.selected_kik_id.is_some() {
                return;
            }
            session.account_id.clone()
        };
        let Some(policy) = self.account(&account_id) else {
            return;
        };
        let Some(kik_id) = self.latest_online_kik_id(&policy).await else {
            return;
        };

        // sys_use 可能在候选扫描期间完成；只填充空选择，绝不覆盖用户显式选择。
        let mut session = session.lock().await;
        if session.selected_kik_id.is_none() {
            session.selected_kik_id = Some(kik_id);
        }
    }

    async fn latest_online_kik_id(&self, policy: &AccountPolicy) -> Option<String> {
        let snapshot = self
            .kiks
            .read()
            .await
            .iter()
            .filter(|(id, _)| policy.allows_kik(id))
            .map(|(id, kik)| (id.clone(), kik.clone()))
            .collect::<Vec<_>>();
        let mut latest: Option<(SystemTime, String)> = None;
        for (id, kik) in snapshot {
            if !kik.initialized() || !kik.exist_kik_conn().await {
                continue;
            }
            let online_at = *kik.kik_client_info.recent_online_time.read().await;
            let replace = latest.as_ref().is_none_or(|(latest_at, latest_id)| {
                online_at > *latest_at || (online_at == *latest_at && id > *latest_id)
            });
            if replace {
                latest = Some((online_at, id));
            }
        }
        latest.map(|(_, id)| id)
    }

    /// 新 Kik 上线时，只为没有当前目标的会话补默认值；账号 ACL 仍是硬边界。
    async fn select_kik_for_unassigned_sessions(&self, kik_id: &str) {
        let sessions = self
            .sessions
            .read()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for session in sessions {
            let mut session = session.lock().await;
            if session.selected_kik_id.is_none()
                && self
                    .account(&session.account_id)
                    .is_some_and(|policy| policy.allows_kik(kik_id))
            {
                session.selected_kik_id = Some(kik_id.to_string());
            }
        }
    }

    /// Kik 完整下线后清除引用它的会话，并立即选择仍在线的最近候选。
    async fn replace_offline_kik_selections(&self, kik_id: &str) {
        let sessions = self
            .sessions
            .read()
            .await
            .iter()
            .map(|(id, session)| (id.clone(), session.clone()))
            .collect::<Vec<_>>();
        let mut cleared = Vec::new();
        for (session_id, session) in sessions {
            let mut session = session.lock().await;
            if session.selected_kik_id.as_deref() == Some(kik_id) {
                session.selected_kik_id = None;
                cleared.push(session_id);
            }
        }
        for session_id in cleared {
            self.ensure_session_has_selected_kik(&session_id).await;
        }
    }

    async fn policy_for_session(&self, session_id: &str) -> anyhow::Result<AccountPolicy> {
        let session = self
            .sessions
            .read()
            .await
            .get(session_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("控制会话已失效"))?;
        let account_id = session.lock().await.account_id.clone();
        self.account(&account_id)
            .ok_or_else(|| anyhow::anyhow!("控制账号已失效"))
    }

    pub async fn get_can_ctrl_kik(&self, session_id: &str) -> anyhow::Result<Vec<(String, Kik)>> {
        let policy = self.policy_for_session(session_id).await?;
        let snapshot = self
            .kiks
            .read()
            .await
            .iter()
            .map(|(id, kik)| (id.clone(), kik.clone()))
            .collect::<Vec<_>>();
        let mut result = Vec::new();
        for (id, kik) in snapshot {
            if policy.allows_kik(&id) && kik.initialized() && kik.exist_kik_conn().await {
                result.push((id, kik));
            }
        }
        Ok(result)
    }

    pub async fn kik_presence_for_session(
        &self,
        session_id: &str,
        kik_id: Option<&str>,
    ) -> anyhow::Result<Vec<KikPresenceVo>> {
        let policy = self.policy_for_session(session_id).await?;
        if kik_id.is_some_and(|id| !policy.allows_kik(id)) {
            return Ok(Vec::new());
        }
        Ok(self
            .kik_presence(kik_id)
            .await
            .into_iter()
            .filter(|record| policy.allows_kik(&record.id))
            .collect())
    }

    /// 为数据密集命令生成服务端内部 ID，并把客户端可控 ID 与目标会话/Kik 绑定。
    pub async fn prepare_data_route(
        &self,
        session_id: &str,
        kik_id: &str,
        cmd: Command,
    ) -> anyhow::Result<Command> {
        let (external_id, wire_id, direction, single_frame, rewritten) = match cmd {
            Command::Ctrl(CtrlCommand::SetFile(external, path)) => {
                validate_data_id(&external)?;
                let wire = Uuid::new_v4().to_string();
                (
                    external,
                    wire.clone(),
                    DataDirection::CtrlToKik,
                    true,
                    Command::Ctrl(CtrlCommand::SetFile(wire, path)),
                )
            }
            Command::Ctrl(CtrlCommand::SetBigFile(external, total, hash, path)) => {
                validate_data_id(&external)?;
                let wire = Uuid::new_v4().to_string();
                (
                    external,
                    wire.clone(),
                    DataDirection::CtrlToKik,
                    false,
                    Command::Ctrl(CtrlCommand::SetBigFile(wire, total, hash, path)),
                )
            }
            Command::Ctrl(CtrlCommand::GetFile(path, _)) => {
                let id = Uuid::new_v4().to_string();
                (
                    id.clone(),
                    id.clone(),
                    DataDirection::KikToCtrl,
                    true,
                    Command::Ctrl(CtrlCommand::GetFile(path, id)),
                )
            }
            Command::Ctrl(CtrlCommand::GetBigFile(path, _)) => {
                let id = Uuid::new_v4().to_string();
                (
                    id.clone(),
                    id.clone(),
                    DataDirection::KikToCtrl,
                    false,
                    Command::Ctrl(CtrlCommand::GetBigFile(path, id)),
                )
            }
            Command::Ctrl(CtrlCommand::Screen(_)) => {
                let id = Uuid::new_v4().to_string();
                (
                    id.clone(),
                    id.clone(),
                    DataDirection::KikToCtrl,
                    true,
                    Command::Ctrl(CtrlCommand::Screen(id)),
                )
            }
            other => return Ok(other),
        };
        self.insert_data_route(
            external_id,
            DataRoute {
                session_id: session_id.to_string(),
                kik_id: kik_id.to_string(),
                wire_data_id: wire_id,
                direction,
                single_frame,
                expires_at: Instant::now() + DATA_ROUTE_TTL,
            },
        )
        .await?;
        Ok(rewritten)
    }

    async fn insert_data_route(&self, id: String, route: DataRoute) -> anyhow::Result<()> {
        let mut routes = self.data_routes.lock().await;
        let now = Instant::now();
        routes.retain(|_, route| route.expires_at > now);
        if routes.len() >= MAX_DATA_ROUTES
            || routes
                .values()
                .filter(|existing| existing.session_id == route.session_id)
                .count()
                >= MAX_SESSION_DATA_ROUTES
        {
            anyhow::bail!("活动数据传输达到上限");
        }
        let key = match route.direction {
            DataDirection::CtrlToKik => ctrl_route_key(&route.session_id, &id),
            DataDirection::KikToCtrl => kik_route_key(&id),
        };
        if routes.contains_key(&key) {
            anyhow::bail!("数据关联 ID 重复");
        }
        routes.insert(key, route);
        drop(routes);
        self.data_route_notify.notify_waiters();
        Ok(())
    }

    /// 把控制端上传使用的外部数据 ID 解析为目标 Kik 和内部 wire ID。
    pub async fn ctrl_data_target(
        &self,
        session_id: &str,
        external_id: &str,
    ) -> Option<(Kik, String, bool)> {
        let route = self
            .route(
                &ctrl_route_key(session_id, external_id),
                DataDirection::CtrlToKik,
            )
            .await?;
        let kik = self.get_initialized_kik_by_id(&route.kik_id).await?;
        Some((kik, route.wire_data_id, route.single_frame))
    }

    /// Ctrl 命令和 CtrlData 位于独立 TLS 连接，数据帧可能在命令任务登记路由前合法到达。
    /// 等待数量和时间均有硬上限，避免已认证客户端用不存在的 ID 长期占用读任务。
    pub async fn wait_ctrl_data_target(
        &self,
        session_id: &str,
        external_id: &str,
    ) -> Option<(Kik, String, bool)> {
        if let Some(target) = self.ctrl_data_target(session_id, external_id).await {
            return Some(target);
        }
        let _permit = self
            .early_ctrl_data_limit
            .clone()
            .try_acquire_owned()
            .ok()?;
        let deadline = Instant::now() + EARLY_CTRL_DATA_WAIT;
        loop {
            // 先登记通知等待者、再复查路由，避免插入恰好发生在检查与 await 之间时丢通知。
            let notified = self.data_route_notify.notified();
            if let Some(target) = self.ctrl_data_target(session_id, external_id).await {
                return Some(target);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() || tokio::time::timeout(remaining, notified).await.is_err() {
                return None;
            }
        }
    }

    /// 校验 Kik 下载帧的发送者与 wire ID，并恢复目标控制会话。
    pub async fn kik_data_target(
        &self,
        kik_id: &str,
        wire_id: &str,
    ) -> Option<(String, String, bool)> {
        let route = self
            .route(&kik_route_key(wire_id), DataDirection::KikToCtrl)
            .await?;
        if route.kik_id != kik_id || route.wire_data_id != wire_id {
            return None;
        }
        Some((route.session_id, wire_id.to_string(), route.single_frame))
    }

    async fn route(&self, id: &str, direction: DataDirection) -> Option<DataRoute> {
        let mut routes = self.data_routes.lock().await;
        let now = Instant::now();
        routes.retain(|_, route| route.expires_at > now);
        routes
            .get(id)
            .filter(|route| route.direction == direction)
            .cloned()
    }

    pub async fn complete_ctrl_single_frame_route(&self, session_id: &str, id: &str) {
        let mut routes = self.data_routes.lock().await;
        let key = ctrl_route_key(session_id, id);
        if routes.get(&key).is_some_and(|route| route.single_frame) {
            routes.remove(&key);
        }
    }

    pub async fn complete_kik_single_frame_route(&self, id: &str) {
        let mut routes = self.data_routes.lock().await;
        let key = kik_route_key(id);
        if routes.get(&key).is_some_and(|route| route.single_frame) {
            routes.remove(&key);
        }
    }

    pub async fn finish_upload_route(&self, session_id: &str, external_id: Option<&str>) {
        if let Some(id) = external_id {
            self.data_routes
                .lock()
                .await
                .remove(&ctrl_route_key(session_id, id));
        }
    }

    pub async fn finish_download_route(&self, session_id: &str, data_id: &str) {
        let mut routes = self.data_routes.lock().await;
        let key = kik_route_key(data_id);
        if routes.get(&key).is_some_and(|route| {
            route.session_id == session_id && route.direction == DataDirection::KikToCtrl
        }) {
            routes.remove(&key);
        }
    }

    pub async fn get_initialized_kik_by_id(&self, id: &str) -> Option<Kik> {
        self.kiks
            .read()
            .await
            .get(id)
            .filter(|kik| kik.initialized())
            .cloned()
    }

    pub async fn delete_kik_conn_if(&self, id: &str, channel: &SharedChannel) -> bool {
        let mapped = self.kiks.read().await.get(id).cloned();
        if let Some(kik) = mapped {
            return kik.delete_kik_conn_if(channel).await;
        }
        false
    }

    pub async fn delete_kik_if_not_online(&self, kik_id: &str) -> Option<Kik> {
        let kik = self.find_kik(kik_id).await?;
        if !kik.exist_data_channel().await && !kik.exist_kik_conn().await {
            let removed = self.kiks.write().await.remove(kik_id);
            if let Some(kik) = removed.as_ref().filter(|kik| kik.initialized()) {
                self.record_kik_offline(kik).await;
            }
            self.data_routes
                .lock()
                .await
                .retain(|_, route| route.kik_id != kik_id);
            if removed.is_some() {
                self.replace_offline_kik_selections(kik_id).await;
            }
            removed
        } else {
            None
        }
    }

    async fn find_kik(&self, kik_id: &str) -> Option<Kik> {
        self.kiks.read().await.get(kik_id).cloned()
    }

    pub async fn delete_kik_data_conn(&self, data_conn: SharedChannel) {
        let kik_id = data_conn.lock().await.attribute(&KIK_ID).cloned();
        if let Some(kik_id) = kik_id {
            if let Some(kik) = self.kiks.read().await.get(&kik_id).cloned() {
                kik.delete_data_conn(data_conn).await;
            }
        }
    }

    pub async fn record_kik_online(&self, kik: &Kik) {
        let Some(id) = kik.id().map(str::to_owned) else {
            return;
        };
        let record = KikPresenceVo {
            id: id.clone(),
            name: kik.kik_client_info.kik_info.name.clone(),
            ip: kik.kik_client_info.ip.read().await.clone(),
            online: true,
            recent_online_unix_ms: unix_time_millis(
                *kik.kik_client_info.recent_online_time.read().await,
            ),
            recent_offline_unix_ms: None,
        };
        let mut records = self.kik_presence.write().await;
        let previous_offline = records
            .get(&id)
            .and_then(|record| record.recent_offline_unix_ms);
        evict_oldest_presence_record(&mut records, &id);
        records.insert(
            id.clone(),
            KikPresenceVo {
                recent_offline_unix_ms: previous_offline,
                ..record
            },
        );
        drop(records);
        self.select_kik_for_unassigned_sessions(&id).await;
    }

    async fn record_kik_offline(&self, kik: &Kik) {
        let Some(id) = kik.id().map(str::to_owned) else {
            return;
        };
        let mut records = self.kik_presence.write().await;
        if let Some(record) = records.get_mut(&id) {
            record.online = false;
            record.recent_offline_unix_ms = Some(unix_time_millis(SystemTime::now()));
        }
    }

    pub async fn kik_presence(&self, kik_id: Option<&str>) -> Vec<KikPresenceVo> {
        let records = self.kik_presence.read().await;
        if let Some(kik_id) = kik_id {
            return records.get(kik_id).cloned().into_iter().collect();
        }
        let mut values = records.values().cloned().collect::<Vec<_>>();
        values.sort_unstable_by(|left, right| {
            right
                .recent_offline_unix_ms
                .unwrap_or(right.recent_online_unix_ms)
                .cmp(
                    &left
                        .recent_offline_unix_ms
                        .unwrap_or(left.recent_online_unix_ms),
                )
        });
        values
    }
}

fn validate_data_id(id: &str) -> anyhow::Result<()> {
    Uuid::parse_str(id)
        .map(|_| ())
        .map_err(|_| anyhow::anyhow!("数据关联 ID 必须是 UUID"))
}

fn ctrl_route_key(session_id: &str, data_id: &str) -> String {
    format!("c:{session_id}:{data_id}")
}

fn kik_route_key(data_id: &str) -> String {
    format!("k:{data_id}")
}

fn evict_oldest_presence_record(records: &mut HashMap<String, KikPresenceVo>, incoming_id: &str) {
    if records.len() < MAX_KIK_PRESENCE_RECORDS || records.contains_key(incoming_id) {
        return;
    }
    let oldest = records
        .iter()
        .filter(|(_, record)| !record.online)
        .min_by_key(|(_, record)| {
            record
                .recent_offline_unix_ms
                .unwrap_or(record.recent_online_unix_ms)
        })
        .map(|(id, _)| id.clone())
        .or_else(|| {
            records
                .iter()
                .min_by_key(|(_, record)| record.recent_online_unix_ms)
                .map(|(id, _)| id.clone())
        });
    if let Some(id) = oldest {
        records.remove(&id);
    }
}

fn unix_time_millis(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
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

    async fn session(context: &Context, account: &str, instance: &str) -> (String, SharedChannel) {
        let id = Uuid::new_v4().to_string();
        let channel = channel(&id, ChannelType::Ctrl);
        context
            .register_ctrl_session(channel.clone(), id.clone(), account.into(), instance.into())
            .await
            .unwrap();
        (id, channel)
    }

    async fn add_online_kik(context: &Context, id: &str, online_at: SystemTime) -> Kik {
        let kik = Kik::new(
            id,
            "auto-select-test-kik",
            "127.0.0.1".to_string(),
            online_at,
            channel(id, ChannelType::Kik),
        );
        kik.set_kik_initialized(true);
        context
            .kiks
            .write()
            .await
            .insert(id.to_string(), kik.clone());
        context.record_kik_online(&kik).await;
        kik
    }

    #[tokio::test]
    async fn instances_are_independent_and_same_instance_reconnects() {
        let context = Context::init();
        let (first, old_channel) = session(&context, "default", "one").await;
        let (second, _) = session(&context, "default", "two").await;
        assert!(context.session_auth_secret(&first).await.is_some());
        assert!(context.session_auth_secret(&second).await.is_some());
        let (replacement, _) = session(&context, "default", "one").await;
        assert!(context.session_auth_secret(&first).await.is_none());
        assert!(context.session_auth_secret(&replacement).await.is_some());
        assert!(!context.delete_ctrl_conn_if(&old_channel).await);
    }

    #[tokio::test]
    async fn concurrent_same_instance_registration_leaves_exactly_one_session() {
        let context = Context::init();
        let mut tasks = Vec::new();
        for index in 0..32 {
            let context = context.clone();
            tasks.push(tokio::spawn(async move {
                session(&context, "default", "same-instance").await;
                index
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        assert_eq!(context.instances.lock().await.len(), 1);
        assert_eq!(context.sessions.read().await.len(), 1);
    }

    #[tokio::test]
    async fn instance_and_account_command_quotas_are_isolated_and_recover_after_release() {
        let registry = AccountRegistry::from_json_or_default(
            Some(
                r#"[
                    {"account_id":"tenant_a","secret":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","allowed_kiks":["*"],"max_instances":2,"max_commands_per_instance":2,"max_commands_per_account":1},
                    {"account_id":"tenant_b","secret":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","allowed_kiks":["*"],"max_instances":1,"max_commands_per_instance":1,"max_commands_per_account":1}
                ]"#,
            ),
            String::new(),
        )
        .unwrap();
        let context = Context::init_with_accounts(registry);
        let (a1, _) = session(&context, "tenant_a", "one").await;
        let (a2, _) = session(&context, "tenant_a", "two").await;
        let (b1, _) = session(&context, "tenant_b", "one").await;

        let a1_permits = context.try_acquire_command(&a1).await.unwrap();
        assert!(context.try_acquire_command(&a2).await.is_err());

        // tenant_a 占满账号配额不能消耗 tenant_b 的账号或实例许可。
        let b1_permits = context.try_acquire_command(&b1).await.unwrap();
        drop(b1_permits);
        drop(a1_permits);

        // 拒绝路径和正常完成路径都必须用 RAII 立即归还全局、账号和实例许可。
        assert!(context.try_acquire_command(&a2).await.is_ok());
    }

    #[tokio::test]
    async fn account_instance_limit_rejects_only_new_instances() {
        let registry = AccountRegistry::from_json_or_default(
            Some(
                r#"[{"account_id":"limited","secret":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","allowed_kiks":["*"],"max_instances":1}]"#,
            ),
            String::new(),
        )
        .unwrap();
        let context = Context::init_with_accounts(registry);
        let (first, _) = session(&context, "limited", "one").await;

        let second_id = Uuid::new_v4().to_string();
        let second_channel = channel(&second_id, ChannelType::Ctrl);
        assert!(context
            .register_ctrl_session(second_channel, second_id, "limited".into(), "two".into(),)
            .await
            .is_err());
        assert!(context.session_auth_secret(&first).await.is_some());

        // 同一 instance 重连是替换而非新增，不应被 max_instances 拒绝。
        let (replacement, _) = session(&context, "limited", "one").await;
        assert!(context.session_auth_secret(&first).await.is_none());
        assert!(context.session_auth_secret(&replacement).await.is_some());
    }

    #[tokio::test]
    async fn per_session_data_route_limit_is_hard_and_recovers_after_cleanup() {
        let context = Context::init();
        let (session_id, _) = session(&context, "default", "routes").await;
        let kik_id = Uuid::new_v4().to_string();
        let mut external_ids = Vec::with_capacity(MAX_SESSION_DATA_ROUTES);

        for _ in 0..MAX_SESSION_DATA_ROUTES {
            let external_id = Uuid::new_v4().to_string();
            context
                .prepare_data_route(
                    &session_id,
                    &kik_id,
                    Command::Ctrl(CtrlCommand::SetFile(external_id.clone(), "target".into())),
                )
                .await
                .unwrap();
            external_ids.push(external_id);
        }
        assert!(context
            .prepare_data_route(
                &session_id,
                &kik_id,
                Command::Ctrl(CtrlCommand::SetFile(
                    Uuid::new_v4().to_string(),
                    "overflow".into(),
                )),
            )
            .await
            .is_err());

        context
            .finish_upload_route(&session_id, external_ids.first().map(String::as_str))
            .await;
        assert!(context
            .prepare_data_route(
                &session_id,
                &kik_id,
                Command::Ctrl(CtrlCommand::SetFile(
                    Uuid::new_v4().to_string(),
                    "recovered".into(),
                )),
            )
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn early_ctrl_data_waiter_observes_route_created_by_command_connection() {
        let context = Context::init();
        let (session_id, _) = session(&context, "default", "early-data").await;
        let kik_id = Uuid::new_v4().to_string();
        let kik = Kik::new(
            &kik_id,
            "tester",
            "127.0.0.1".to_string(),
            SystemTime::now(),
            channel(&kik_id, ChannelType::Kik),
        );
        kik.set_kik_initialized(true);
        context.kiks.write().await.insert(kik_id.clone(), kik);
        let external_id = Uuid::new_v4().to_string();

        let waiter = {
            let context = context.clone();
            let session_id = session_id.clone();
            let external_id = external_id.clone();
            tokio::spawn(async move {
                context
                    .wait_ctrl_data_target(&session_id, &external_id)
                    .await
            })
        };
        tokio::task::yield_now().await;
        context
            .prepare_data_route(
                &session_id,
                &kik_id,
                Command::Ctrl(CtrlCommand::SetFile(external_id, "target".into())),
            )
            .await
            .unwrap();

        assert!(waiter.await.unwrap().is_some());
    }

    #[tokio::test]
    async fn data_nonce_replay_is_rejected_per_session() {
        let context = Context::init();
        let (session, _) = session(&context, "default", "one").await;
        assert!(context.validate_ctrl_data_session(&session, "nonce").await);
        assert!(!context.validate_ctrl_data_session(&session, "nonce").await);
    }

    #[tokio::test]
    async fn kik_online_after_ctrl_is_selected_automatically() {
        let context = Context::init();
        let (session_id, _) = session(&context, "default", "ctrl-first").await;
        assert!(context.get_kik(&session_id).await.is_none());

        let kik_id = Uuid::new_v4().to_string();
        add_online_kik(&context, &kik_id, SystemTime::now()).await;

        assert_eq!(
            context.get_kik(&session_id).await.unwrap().id(),
            Some(kik_id.as_str())
        );
    }

    #[tokio::test]
    async fn ctrl_online_after_kik_selects_latest_accessible_kik() {
        let context = Context::init();
        let first = Uuid::new_v4().to_string();
        let latest = Uuid::new_v4().to_string();
        add_online_kik(
            &context,
            &first,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1),
        )
        .await;
        add_online_kik(
            &context,
            &latest,
            SystemTime::UNIX_EPOCH + Duration::from_secs(2),
        )
        .await;

        let (session_id, _) = session(&context, "default", "kik-first").await;
        assert_eq!(
            context.get_kik(&session_id).await.unwrap().id(),
            Some(latest.as_str())
        );
    }

    #[tokio::test]
    async fn offline_selection_falls_back_to_another_online_kik() {
        let context = Context::init();
        let fallback_id = Uuid::new_v4().to_string();
        let selected_id = Uuid::new_v4().to_string();
        add_online_kik(
            &context,
            &fallback_id,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1),
        )
        .await;
        let selected = add_online_kik(
            &context,
            &selected_id,
            SystemTime::UNIX_EPOCH + Duration::from_secs(2),
        )
        .await;
        let (session_id, _) = session(&context, "default", "fallback").await;
        assert_eq!(
            context.get_kik(&session_id).await.unwrap().id(),
            Some(selected_id.as_str())
        );

        selected.delete_kik_conn().await;
        assert!(context
            .delete_kik_if_not_online(&selected_id)
            .await
            .is_some());
        assert_eq!(
            context.get_kik(&session_id).await.unwrap().id(),
            Some(fallback_id.as_str())
        );
    }

    #[tokio::test]
    async fn automatic_selection_never_crosses_account_acl() {
        let allowed_id = Uuid::new_v4().to_string();
        let denied_id = Uuid::new_v4().to_string();
        let registry = AccountRegistry::from_json_or_default(
            Some(&format!(
                r#"[{{"account_id":"restricted","secret":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","allowed_kiks":["{allowed_id}"]}}]"#
            )),
            String::new(),
        )
        .unwrap();
        let context = Context::init_with_accounts(registry);
        add_online_kik(
            &context,
            &allowed_id,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1),
        )
        .await;
        add_online_kik(
            &context,
            &denied_id,
            SystemTime::UNIX_EPOCH + Duration::from_secs(2),
        )
        .await;

        let (session_id, _) = session(&context, "restricted", "acl").await;
        assert_eq!(
            context.get_kik(&session_id).await.unwrap().id(),
            Some(allowed_id.as_str())
        );
    }

    #[tokio::test]
    async fn kik_presence_never_exceeds_hard_limit() {
        let context = Context::init();
        for index in 0..=MAX_KIK_PRESENCE_RECORDS {
            let id = format!("kik-{index}");
            let kik = Kik::new(
                &id,
                "tester",
                "127.0.0.1".to_string(),
                SystemTime::UNIX_EPOCH + Duration::from_secs(index as u64),
                channel(&id, ChannelType::Kik),
            );
            kik.set_kik_initialized(true);
            context.record_kik_online(&kik).await;
        }
        assert_eq!(
            context.kik_presence(None).await.len(),
            MAX_KIK_PRESENCE_RECORDS
        );
    }

    #[tokio::test]
    async fn accounts_instances_selections_and_data_ids_are_isolated() {
        let registry = AccountRegistry::from_json_or_default(
            Some(
                r#"[
                    {"account_id":"tenant_a","secret":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","allowed_kiks":["*"]},
                    {"account_id":"tenant_b","secret":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","allowed_kiks":["*"]}
                ]"#,
            ),
            String::new(),
        )
        .unwrap();
        let context = Context::init_with_accounts(registry);
        let (session_a, _) = session(&context, "tenant_a", "instance_a").await;
        let (session_b, _) = session(&context, "tenant_b", "instance_b").await;
        let kik_a_id = Uuid::new_v4().to_string();
        let kik_b_id = Uuid::new_v4().to_string();
        for id in [&kik_a_id, &kik_b_id] {
            let kik = Kik::new(
                id,
                "tester",
                "127.0.0.1".to_string(),
                SystemTime::now(),
                channel(id, ChannelType::Kik),
            );
            kik.set_kik_initialized(true);
            context.kiks.write().await.insert(id.clone(), kik);
        }
        context.set_kik(&session_a, &kik_a_id).await.unwrap();
        context.set_kik(&session_b, &kik_b_id).await.unwrap();
        assert_eq!(
            context.get_kik(&session_a).await.unwrap().id(),
            Some(kik_a_id.as_str())
        );
        assert_eq!(
            context.get_kik(&session_b).await.unwrap().id(),
            Some(kik_b_id.as_str())
        );

        // 客户端关联 ID 不是全局身份；不同会话可安全复用，服务端会分别改写内部 ID。
        let external_id = Uuid::new_v4().to_string();
        context
            .prepare_data_route(
                &session_a,
                &kik_a_id,
                Command::Ctrl(CtrlCommand::SetFile(external_id.clone(), "a".into())),
            )
            .await
            .unwrap();
        context
            .prepare_data_route(
                &session_b,
                &kik_b_id,
                Command::Ctrl(CtrlCommand::SetFile(external_id.clone(), "b".into())),
            )
            .await
            .unwrap();
        assert_eq!(
            context
                .ctrl_data_target(&session_a, &external_id)
                .await
                .unwrap()
                .0
                .id(),
            Some(kik_a_id.as_str())
        );
        assert!(context
            .ctrl_data_target(&session_b, &external_id)
            .await
            .is_some());
        assert!(context
            .prepare_data_route(
                &session_a,
                &kik_b_id,
                Command::Ctrl(CtrlCommand::SetFile(external_id.clone(), "replace".into())),
            )
            .await
            .is_err());
        assert_eq!(
            context
                .ctrl_data_target(&session_a, &external_id)
                .await
                .unwrap()
                .0
                .id(),
            Some(kik_a_id.as_str())
        );
    }
}
