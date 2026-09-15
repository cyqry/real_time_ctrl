//! 控制端连接、响应路由、数据队列和自动重连的共享状态。
//!
//! `Agent` 拥有 Ctrl 主连接，`Context` 在其外层管理多条 CtrlData 连接。每个命令 ID 对应独立 oneshot，
//! 每个数据 ID 对应独立有界队列，因此 HTTP 请求可以乱序完成而不会争抢同一个 Receiver。

use crate::ctrl_conn::ctrl_conn;
use crate::ctrl_data_conn::ctrl_data_conn;
use bytes::BytesMut;
use common::channel::{Channel, DATA_CONNECTION_RECOVERY_TIMEOUT};
use common::config::Config;
use common::protocol::ReqCmd;
use ctrl_common::ctrl_frame::encode_data_frame;
use ctrl_common::ctrl_protocol::ctrl_cmd_req;
use ctrl_common::ctrl_protocol::ctrl_data_ack;
use ctrl_common::ctrl_resp::CmdResp;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::{
    oneshot, Mutex, Notify, OwnedSemaphorePermit, RwLock, Semaphore, TryAcquireError,
};
use tokio::task::JoinHandle;
use tokio::time;
use uuid::Uuid;

type DataMessage = (String, BytesMut);
const DATA_QUEUE_CAPACITY: usize = 2;
const DATA_QUEUE_SEND_TIMEOUT: Duration = Duration::from_secs(30);
const CONTROL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(6 * 60);
const LONG_CONTROL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60 + 10 * 60);
const DESIRED_DATA_CONNECTIONS: usize = 3;
const MAX_PARALLEL_API_COMMANDS: usize = 32;
const MAX_ACTIVE_DATA_ROUTES: usize = 64;
const MAX_PRE_REGISTERED_DATA_ROUTES: usize = 16;
const MAX_PRE_REGISTERED_DATA_BYTES: usize = 64 * 1024 * 1024;
const PRE_REGISTERED_DATA_TTL: Duration = Duration::from_secs(30);
const DATA_RECONNECT_MAX_BACKOFF: Duration = Duration::from_secs(30);
const DATA_CONNECTION_STABLE_AFTER: Duration = Duration::from_secs(30);

/// 控制响应按关联 ID 定向投递，避免并行请求争抢同一个 Receiver。
#[derive(Clone, Default)]
pub(crate) struct ResponseRouter {
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<CmdResp>>>>,
}

impl ResponseRouter {
    pub async fn register(&self, id: &str) -> anyhow::Result<oneshot::Receiver<CmdResp>> {
        let mut pending = self.pending.lock().await;
        if pending.len() >= MAX_PARALLEL_API_COMMANDS {
            anyhow::bail!("活动控制请求达到上限");
        }
        if pending.contains_key(id) {
            anyhow::bail!("控制请求关联 ID 重复");
        }
        let (tx, rx) = oneshot::channel();
        pending.insert(id.to_string(), tx);
        Ok(rx)
    }

    pub async fn deliver(&self, response: CmdResp) {
        if let Some(sender) = self.pending.lock().await.remove(response.get_cmd_id()) {
            let _ = sender.send(response);
        }
    }

    pub async fn cancel(&self, id: &str) {
        self.pending.lock().await.remove(id);
    }

    pub async fn fail_all(&self) {
        self.pending.lock().await.clear();
    }
}

/// 一个数据 ID 的本地有界收件箱。
///
/// 数据可能早于控制响应到达，此时先创建 `claimed=false` 的预登记路由；命令处理开始等待后再认领。
struct DataRoute {
    /// 由数据连接读循环投递帧。
    tx: Sender<DataMessage>,
    /// 由拥有该数据 ID 的命令独占消费。
    rx: Arc<Mutex<Receiver<DataMessage>>>,
    /// 是否已有命令声明拥有该 ID；未认领路由受更严格的数量、字节和 TTL 限制。
    claimed: bool,
    created_at: Instant,
    /// 未认领阶段累计的 payload 大小，用于限制乱序缓冲内存。
    pre_registered_bytes: usize,
}

#[derive(Clone)]
/// 所有 `RealCtrlApi` 克隆共享的进程级命令并发门禁。
struct CommandGate(Arc<Semaphore>);

impl CommandGate {
    fn new() -> Self {
        Self(Arc::new(Semaphore::new(MAX_PARALLEL_API_COMMANDS)))
    }

    fn try_acquire(&self) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        self.0.clone().try_acquire_owned()
    }
}

#[derive(Clone)]
/// 控制端所有入口共享的运行上下文。
///
/// 克隆只复制 `Arc`，不会创建新的远程会话或绕过并发门禁。
pub struct Context {
    /// 当前 Ctrl 主连接及其认证 session；重连时原地替换。
    pub agent: Arc<RwLock<Agent>>,
    /// 多条 CtrlData 连接，以连接随机 ID 为键。
    data_conns: Arc<RwLock<HashMap<String, Arc<Mutex<Channel>>>>>,
    /// 数据连接入池时唤醒等待发送或等待启动完成的任务。
    data_connection_notify: Arc<Notify>,
    /// 三个长期监督槽位；主会话换代时先取消旧槽位，再为新 session 创建槽位。
    data_supervisors: Arc<Mutex<Vec<JoinHandle<()>>>>,
    /// 数据发送轮询游标，不参与安全判断。
    next_data_conn: Arc<AtomicUsize>,
    /// 数据 ID 到私有收件箱的映射。
    data_routes: Arc<Mutex<HashMap<String, DataRoute>>>,
    command_gate: CommandGate,
    /// 连接故障时保证只有一个请求执行重连。
    reconnect_gate: Arc<Mutex<()>>,
}

#[derive(Clone)]
/// 一条已认证 Ctrl 主连接及其响应路由器。
pub struct Agent {
    pub config: Config,
    pub session_id: Option<String>,
    conn: Arc<Mutex<Channel>>,
    responses: ResponseRouter,
}

impl Context {
    pub fn new(agent: Arc<RwLock<Agent>>) -> Self {
        Self {
            agent,
            data_conns: Arc::new(RwLock::new(HashMap::new())),
            data_connection_notify: Arc::new(Notify::new()),
            data_supervisors: Arc::new(Mutex::new(Vec::new())),
            next_data_conn: Arc::new(AtomicUsize::new(0)),
            data_routes: Arc::new(Mutex::new(HashMap::new())),
            command_gate: CommandGate::new(),
            reconnect_gate: Arc::new(Mutex::new(())),
        }
    }

    pub fn try_acquire_command(&self) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        self.command_gate.try_acquire()
    }

    pub async fn insert_ctrl_data_conn(
        &self,
        expected_session_id: &str,
        data_conn: Arc<Mutex<Channel>>,
    ) -> anyhow::Result<()> {
        let id = {
            let connection = data_conn.lock().await;
            if connection.is_closed() {
                anyhow::bail!("拒绝把已经关闭的数据连接加入连接池");
            }
            connection.require_id()?.to_string()
        };
        if !self.is_current_session(expected_session_id).await {
            anyhow::bail!("数据连接属于已经结束的控制会话");
        }
        self.data_conns.write().await.insert(id, data_conn);
        // 数据池恢复时广播给所有并发 API 请求；每个请求醒来后仍会自行复查健康连接。
        self.data_connection_notify.notify_waiters();
        Ok(())
    }

    /// session ID 由服务端随机生成，可用来阻止旧数据握手跨越主连接重连边界。
    async fn is_current_session(&self, expected_session_id: &str) -> bool {
        self.agent
            .read()
            .await
            .session_id
            .as_deref()
            .is_some_and(|current| current == expected_session_id)
    }

    pub async fn delete_ctrl_data_conn(&self, data_conn: Arc<Mutex<Channel>>) {
        let id = data_conn.lock().await.id().map(str::to_owned);
        if let Some(id) = id {
            self.data_conns.write().await.remove(&id);
        }
    }

    /// 把 CtrlData 收到的帧投递给所属命令；允许在命令认领路由前有界暂存。
    pub async fn enqueue_data(&self, message: DataMessage) -> anyhow::Result<()> {
        let (sender, cleanup) = {
            let mut routes = self.data_routes.lock().await;
            let now = Instant::now();
            routes.retain(|_, route| {
                route.claimed
                    || now.saturating_duration_since(route.created_at) <= PRE_REGISTERED_DATA_TTL
            });
            let pending_count = routes.values().filter(|route| !route.claimed).count();
            let pending_bytes = routes
                .values()
                .filter(|route| !route.claimed)
                .map(|route| route.pre_registered_bytes)
                .sum::<usize>();
            if let Some(route) = routes.get_mut(&message.0) {
                if !route.claimed {
                    if pending_bytes.saturating_add(message.1.len()) > MAX_PRE_REGISTERED_DATA_BYTES
                    {
                        anyhow::bail!("控制端预到达数据缓冲达到上限");
                    }
                    route.pre_registered_bytes =
                        route.pre_registered_bytes.saturating_add(message.1.len());
                }
                (route.tx.clone(), None)
            } else {
                if routes.len() >= MAX_ACTIVE_DATA_ROUTES
                    || pending_count >= MAX_PRE_REGISTERED_DATA_ROUTES
                    || pending_bytes.saturating_add(message.1.len()) > MAX_PRE_REGISTERED_DATA_BYTES
                {
                    anyhow::bail!("控制端预到达数据路由达到上限");
                }
                let key = message.0.clone();
                let created_at = Instant::now();
                let (tx, rx) = channel(DATA_QUEUE_CAPACITY);
                routes.insert(
                    key.clone(),
                    DataRoute {
                        tx: tx.clone(),
                        rx: Arc::new(Mutex::new(rx)),
                        claimed: false,
                        created_at,
                        pre_registered_bytes: message.1.len(),
                    },
                );
                (tx, Some((key, created_at)))
            }
        };
        if let Some((key, created_at)) = cleanup {
            let routes = self.data_routes.clone();
            tokio::spawn(async move {
                tokio::time::sleep(PRE_REGISTERED_DATA_TTL).await;
                let mut routes = routes.lock().await;
                if routes
                    .get(&key)
                    .is_some_and(|route| !route.claimed && route.created_at == created_at)
                {
                    routes.remove(&key);
                }
            });
        }
        tokio::time::timeout(DATA_QUEUE_SEND_TIMEOUT, sender.send(message))
            .await
            .map_err(|_| anyhow::anyhow!("控制端数据队列持续拥塞"))?
            .map_err(|_| anyhow::anyhow!("控制端数据接收任务已关闭"))
    }

    pub async fn send_data(&self, data: &[u8]) -> anyhow::Result<String> {
        let id = Uuid::new_v4().to_string();
        self.send_data_with_id(&id, data).await?;
        Ok(id)
    }

    /// 在数据连接池中轮询发送完整帧；首选连接失败时尝试其余健康连接。
    pub async fn send_data_with_id(&self, data_id: &str, data: &[u8]) -> anyhow::Result<()> {
        let encoded = encode_data_frame(data_id, data)?;
        let deadline = Instant::now() + DATA_CONNECTION_RECOVERY_TIMEOUT;
        let mut last_error = None;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let connections = self.wait_data_connections_for_send(remaining).await;
            if connections.is_empty() {
                break;
            }
            for connection in connections {
                let mut connection = connection.lock().await;
                match connection.write_and_flush(&encoded).await {
                    Ok(()) => return Ok(()),
                    Err(error) => {
                        // 失败的 write_all 可能已经留下半帧，必须关闭本连接；完整帧可在新连接上重试。
                        connection.try_write_half_close().await;
                        last_error = Some(error);
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("应用数据传输通道在恢复时限内不可用")))
    }

    /// 认领指定数据 ID 并等待下一帧；同一命令可重复调用以消费大文件分片。
    pub async fn wait_data(&self, data_id: &str) -> anyhow::Result<BytesMut> {
        let receiver = self.claim_data_route(data_id).await?;
        tokio::time::timeout(Duration::from_secs(6 * 60), async {
            receiver
                .lock()
                .await
                .recv()
                .await
                .map(|(_, data)| data)
                .ok_or_else(|| anyhow::Error::msg("数据通道已关闭"))
        })
        .await
        .map_err(|_| anyhow::anyhow!("等待数据响应超时"))?
    }

    async fn claim_data_route(
        &self,
        data_id: &str,
    ) -> anyhow::Result<Arc<Mutex<Receiver<DataMessage>>>> {
        let mut routes = self.data_routes.lock().await;
        if let Some(route) = routes.get_mut(data_id) {
            route.claimed = true;
            route.pre_registered_bytes = 0;
            return Ok(route.rx.clone());
        }
        if routes.len() >= MAX_ACTIVE_DATA_ROUTES {
            anyhow::bail!("活动数据传输达到上限");
        }
        let (tx, rx) = channel(DATA_QUEUE_CAPACITY);
        let rx = Arc::new(Mutex::new(rx));
        routes.insert(
            data_id.to_string(),
            DataRoute {
                tx: tx.clone(),
                rx: rx.clone(),
                claimed: true,
                created_at: Instant::now(),
                pre_registered_bytes: 0,
            },
        );
        Ok(rx)
    }

    /// 删除本地收件箱并通知服务端释放对应下载路由。
    pub async fn finish_data_route(&self, data_id: &str) {
        self.data_routes.lock().await.remove(data_id);
        let agent = self.agent.read().await.clone();
        let mut connection = agent.conn.lock().await;
        if !connection.is_closed() {
            let _ = connection
                .write_and_flush(&ctrl_data_ack(data_id.to_string()))
                .await;
        }
    }

    async fn data_connections_for_send(&self) -> Vec<Arc<Mutex<Channel>>> {
        let data_map = self.data_conns.read().await;
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

    /// 等待自动补建任务恢复至少一条健康 CtrlData 连接。
    ///
    /// 先创建 `notified` 再检查连接池，避免连接恰好在两步之间入池而丢失通知。这里不持有连接池锁
    /// 等待网络连接锁，因此不会阻塞其他连接的加入和清理。
    pub async fn wait_data_connections_for_send(
        &self,
        wait_timeout: Duration,
    ) -> Vec<Arc<Mutex<Channel>>> {
        let deadline = Instant::now() + wait_timeout;
        loop {
            let notified = self.data_connection_notify.notified();
            tokio::pin!(notified);
            // 先把 future 注册进 Notify 的等待队列，再检查连接池，避免广播发生在检查与 await 之间。
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

    /// 启动三个长期 CtrlData 监督槽位；至少一条首次连接成功即可降级运行。
    ///
    /// 旧实现只并行连接一次，后续单条数据连接退出便永久减少池容量。现在每个槽位都会等待自己的
    /// 连接任务结束，然后指数退避补建；主 session 换代时由 `reset_data_connections` 整体取消旧槽位。
    pub async fn data_init(&self) -> anyhow::Result<()> {
        self.stop_data_supervisors().await;
        let (config, session_id) = {
            let agent = self.agent.read().await;
            let session_id = agent
                .session_id
                .clone()
                .ok_or_else(|| anyhow::anyhow!("控制会话尚未建立，无法初始化数据连接"))?;
            (agent.config.clone(), session_id)
        };
        let (first_result_tx, mut first_result_rx) =
            channel::<Result<(), String>>(DESIRED_DATA_CONNECTIONS);
        let mut supervisors = Vec::with_capacity(DESIRED_DATA_CONNECTIONS);
        for slot in 0..DESIRED_DATA_CONNECTIONS {
            supervisors.push(tokio::spawn(supervise_ctrl_data_connection(
                self.clone(),
                config.clone(),
                session_id.clone(),
                slot,
                first_result_tx.clone(),
            )));
        }
        drop(first_result_tx);

        let mut completed_attempts = 0;
        let mut last_error = None::<String>;
        while let Some(result) = first_result_rx.recv().await {
            completed_attempts += 1;
            match result {
                Ok(()) => {
                    *self.data_supervisors.lock().await = supervisors;
                    return Ok(());
                }
                Err(error) => last_error = Some(error),
            }
            if completed_attempts == DESIRED_DATA_CONNECTIONS {
                break;
            }
        }
        for supervisor in supervisors {
            supervisor.abort();
            let _ = supervisor.await;
        }
        Err(anyhow::anyhow!(last_error.unwrap_or_else(|| {
            "数据通道初始化任务意外结束".to_string()
        })))
    }

    pub async fn request(&self, cmd: &ReqCmd) -> anyhow::Result<CmdResp> {
        self.request_after_send(cmd, None).await
    }

    /// 发出命令，并在控制帧写成功后可选地放行关联上传任务。
    ///
    /// 连接故障会尝试恢复未来请求所需的连接，但绝不会自动重放本次已可能执行的命令。
    pub async fn request_after_send(
        &self,
        cmd: &ReqCmd,
        transfer_start: Option<oneshot::Sender<()>>,
    ) -> anyhow::Result<CmdResp> {
        let failed_agent = self.agent.read().await.clone();
        let request_error = match failed_agent.req(cmd, transfer_start).await {
            Ok(response) => return Ok(response),
            Err(error) => error,
        };
        if !failed_agent.conn.lock().await.is_closed() {
            // 单条命令超时或本地数据任务失败不代表共享连接失效，不能牵连其他并行请求。
            return Err(request_error);
        }

        // 多个失败请求只允许一个执行重连；其他请求观察到新连接后直接返回“结果未知”。
        let _reconnect = self.reconnect_gate.lock().await;
        if !Arc::ptr_eq(&self.agent.read().await.conn, &failed_agent.conn) {
            anyhow::bail!("连接已由其他请求恢复；为避免重复执行，本次命令未自动重放");
        }
        if self.agent.write().await.re_conn(2).await.is_ok() {
            self.reset_data_connections().await;
            self.data_init()
                .await
                .map_err(|error| anyhow::anyhow!("控制连接已恢复，但数据通道恢复失败: {error}"))?;
            anyhow::bail!(
                "连接已恢复；为避免重复执行，本次命令未自动重放，请先查询状态再决定是否重试"
            );
        }
        Err(anyhow::anyhow!("控制连接中断且自动恢复失败"))
    }

    async fn reset_data_connections(&self) {
        self.stop_data_supervisors().await;
        let old_connections = self
            .data_conns
            .write()
            .await
            .drain()
            .map(|(_, channel)| channel)
            .collect::<Vec<_>>();
        self.data_routes.lock().await.clear();
        for channel in old_connections {
            channel.lock().await.try_write_half_close().await;
        }
    }

    /// 取消并等待全部旧监督槽位退出，避免旧 session 在新主连接建立后继续补建数据连接。
    async fn stop_data_supervisors(&self) {
        let supervisors = std::mem::take(&mut *self.data_supervisors.lock().await);
        for supervisor in &supervisors {
            supervisor.abort();
        }
        for supervisor in supervisors {
            let _ = supervisor.await;
        }
    }
}

/// 维护一个 CtrlData 连接槽位，直到主 session 被替换或监督任务被取消。
async fn supervise_ctrl_data_connection(
    context: Context,
    config: Config,
    session_id: String,
    slot: usize,
    first_result_tx: Sender<Result<(), String>>,
) {
    let mut first_result_tx = Some(first_result_tx);
    let mut backoff = Duration::from_secs(1);
    loop {
        if !context.is_current_session(&session_id).await {
            return;
        }
        let connected_at = Instant::now();
        match ctrl_data_conn(context.clone(), &config, &session_id).await {
            Ok(connection_task) => {
                if let Some(sender) = first_result_tx.take() {
                    let _ = sender.send(Ok(())).await;
                }
                let _ = connection_task.await;
                if connected_at.elapsed() >= DATA_CONNECTION_STABLE_AFTER {
                    backoff = Duration::from_secs(1);
                }
            }
            Err(error) => {
                let first_attempt = first_result_tx.is_some();
                if let Some(sender) = first_result_tx.take() {
                    let _ = sender.send(Err(error.to_string())).await;
                }
                if first_attempt {
                    log::warn!("CtrlData 连接槽位 {slot} 首次建立失败: {error}");
                } else {
                    // 长期故障按最高 30 秒退避重试；后续事件降为 debug，避免离线期间持续刷生产日志。
                    log::debug!("CtrlData 连接槽位 {slot} 补建失败: {error}");
                }
            }
        }

        if !context.is_current_session(&session_id).await {
            return;
        }
        let stagger = Duration::from_millis((slot as u64) * 250);
        time::sleep(backoff + stagger).await;
        backoff = backoff
            .checked_mul(2)
            .unwrap_or(DATA_RECONNECT_MAX_BACKOFF)
            .min(DATA_RECONNECT_MAX_BACKOFF);
    }
}

impl Agent {
    /// 建立 pinned TLS 主连接并完成 HMAC 认证，返回可供多个请求共享的 Agent。
    pub async fn create(config: &Config) -> anyhow::Result<Self> {
        let responses = ResponseRouter::default();
        let (conn, session_id) = ctrl_conn(config, responses.clone()).await?;
        Ok(Self {
            config: config.clone(),
            session_id,
            conn,
            responses,
        })
    }

    pub async fn close(&mut self) {
        let _ = self.conn.clone().lock().await.write_half_close().await;
        self.responses.fail_all().await;
    }

    pub async fn re_conn(&mut self, retry_count: u32) -> anyhow::Result<()> {
        let mut last_error = None;
        for _ in 0..retry_count {
            let responses = ResponseRouter::default();
            match ctrl_conn(&self.config, responses.clone()).await {
                Ok((conn, session_id)) => {
                    self.responses.fail_all().await;
                    self.conn = conn;
                    self.responses = responses;
                    self.session_id = session_id;
                    return Ok(());
                }
                Err(error) => {
                    last_error = Some(error);
                    time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("控制连接重试次数必须大于 0")))
    }

    /// 注册响应等待者、写出控制帧并等待匹配 ID 的响应。
    ///
    /// 必须先注册再写帧，否则极快响应可能在等待者出现前到达而被丢弃。
    pub async fn req(
        &self,
        cmd: &ReqCmd,
        transfer_start: Option<oneshot::Sender<()>>,
    ) -> anyhow::Result<CmdResp> {
        let response_rx = self.responses.register(cmd.get_id()).await?;
        if let Err(error) = self
            .conn
            .lock()
            .await
            .write_and_flush(&ctrl_cmd_req(cmd.clone()))
            .await
        {
            self.responses.cancel(cmd.get_id()).await;
            return Err(error);
        }
        if transfer_start.is_some_and(|sender| sender.send(()).is_err()) {
            self.responses.cancel(cmd.get_id()).await;
            anyhow::bail!("控制命令已写出，但关联数据发送任务已提前退出");
        }

        let response_timeout = if cmd.get_cmd_options().timeout() {
            CONTROL_RESPONSE_TIMEOUT
        } else {
            LONG_CONTROL_RESPONSE_TIMEOUT
        };
        let result = tokio::time::timeout(response_timeout, response_rx)
            .await
            .map_err(|_| anyhow::anyhow!("等待控制响应超过命令总时限"))?
            .map_err(|_| anyhow::anyhow!("控制响应通道已关闭"));
        if result.is_err() {
            self.responses.cancel(cmd.get_id()).await;
        }
        result
    }
}

pub fn id() -> String {
    Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::{CommandGate, ResponseRouter, MAX_PARALLEL_API_COMMANDS};
    use ctrl_common::ctrl_resp::{CmdResp, Resp, ServerResp};

    #[test]
    fn command_gate_allows_bounded_parallel_commands() {
        let gate = CommandGate::new();
        let permits = (0..MAX_PARALLEL_API_COMMANDS)
            .map(|_| gate.try_acquire().unwrap())
            .collect::<Vec<_>>();
        assert!(gate.try_acquire().is_err());
        drop(permits);
        assert!(gate.try_acquire().is_ok());
    }

    #[tokio::test]
    async fn response_router_delivers_out_of_order_responses_to_the_right_request() {
        let router = ResponseRouter::default();
        let first = router.register("first").await.unwrap();
        let second = router.register("second").await.unwrap();

        router
            .deliver(CmdResp::new(
                "second".to_string(),
                Resp::Server(ServerResp::Error(1, "second-response".to_string())),
            ))
            .await;
        router
            .deliver(CmdResp::new(
                "first".to_string(),
                Resp::Server(ServerResp::Error(1, "first-response".to_string())),
            ))
            .await;

        assert_eq!(first.await.unwrap().get_cmd_id(), "first");
        assert_eq!(second.await.unwrap().get_cmd_id(), "second");
    }
}
