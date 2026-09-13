use crate::ctrl_conn::ctrl_conn;
use crate::ctrl_data_conn::ctrl_data_conn;
use bytes::BytesMut;
use common::channel::Channel;
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
use tokio::sync::{oneshot, Mutex, OwnedSemaphorePermit, RwLock, Semaphore, TryAcquireError};
use tokio::task::JoinSet;
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

struct DataRoute {
    tx: Sender<DataMessage>,
    rx: Arc<Mutex<Receiver<DataMessage>>>,
    claimed: bool,
    created_at: Instant,
    pre_registered_bytes: usize,
}

#[derive(Clone)]
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
pub struct Context {
    pub agent: Arc<RwLock<Agent>>,
    data_conns: Arc<RwLock<HashMap<String, Arc<Mutex<Channel>>>>>,
    next_data_conn: Arc<AtomicUsize>,
    data_routes: Arc<Mutex<HashMap<String, DataRoute>>>,
    command_gate: CommandGate,
    reconnect_gate: Arc<Mutex<()>>,
}

#[derive(Clone)]
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
        data_conn: Arc<Mutex<Channel>>,
    ) -> anyhow::Result<()> {
        let id = data_conn.lock().await.require_id()?.to_string();
        self.data_conns.write().await.insert(id, data_conn);
        Ok(())
    }

    pub async fn delete_ctrl_data_conn(&self, data_conn: Arc<Mutex<Channel>>) {
        let id = data_conn.lock().await.id().map(str::to_owned);
        if let Some(id) = id {
            self.data_conns.write().await.remove(&id);
        }
    }

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

    pub async fn send_data_with_id(&self, data_id: &str, data: &[u8]) -> anyhow::Result<()> {
        let encoded = encode_data_frame(data_id, data)?;
        let mut last_error = None;
        for connection in self.data_connections_for_send().await {
            let mut connection = connection.lock().await;
            if connection.is_closed() {
                continue;
            }
            match connection.write_and_flush(&encoded).await {
                Ok(()) => return Ok(()),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("应用数据传输通道未初始化")))
    }

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

    pub async fn find_ctrl_data(&self) -> Option<Arc<Mutex<Channel>>> {
        self.data_connections_for_send().await.into_iter().next()
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

    pub async fn data_init(&self) -> anyhow::Result<()> {
        let config = self.agent.read().await.config.clone();
        let mut attempts = JoinSet::new();
        for _ in 0..DESIRED_DATA_CONNECTIONS {
            let (context, config) = (self.clone(), config.clone());
            attempts.spawn(async move { ctrl_data_conn(context, &config).await });
        }
        let mut connected = 0;
        let mut last_error = None;
        while let Some(result) = attempts.join_next().await {
            match result {
                Ok(Ok(())) => connected += 1,
                Ok(Err(error)) => last_error = Some(error),
                Err(error) => last_error = Some(error.into()),
            }
        }
        if connected == 0 {
            return Err(last_error.unwrap_or_else(|| anyhow::anyhow!("数据通道初始化失败")));
        }
        if connected < DESIRED_DATA_CONNECTIONS {
            log::warn!(
                "仅建立 {connected}/{DESIRED_DATA_CONNECTIONS} 条数据连接，文件传输将降级运行"
            );
        }
        Ok(())
    }

    pub async fn request(&self, cmd: &ReqCmd) -> anyhow::Result<CmdResp> {
        self.request_after_send(cmd, None).await
    }

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
}

impl Agent {
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
