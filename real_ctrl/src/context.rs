use crate::ctrl_conn::ctrl_conn;
use crate::ctrl_data_conn::ctrl_data_conn;
use bytes::BytesMut;
use common::channel::Channel;
use common::config::Config;
use common::protocol::ReqCmd;
use ctrl_common::ctrl_frame::encode_data_frame;
use ctrl_common::ctrl_protocol::ctrl_cmd_req;
use ctrl_common::ctrl_resp::CmdResp;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::{mpsc, oneshot, Mutex, OwnedSemaphorePermit, RwLock, Semaphore, TryAcquireError};
use tokio::task::JoinSet;
use tokio::time;
use uuid::Uuid;

type DataMessage = (String, BytesMut);
type SharedDataReceiver = Arc<Mutex<Receiver<DataMessage>>>;
const DATA_QUEUE_CAPACITY: usize = 2;
const DATA_QUEUE_SEND_TIMEOUT: Duration = Duration::from_secs(30);
const CONTROL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(6 * 60);
const LONG_CONTROL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60 + 10 * 60);
const DESIRED_DATA_CONNECTIONS: usize = 3;

#[derive(Clone)]
struct CommandGate(Arc<Semaphore>);

impl CommandGate {
    fn new() -> Self {
        Self(Arc::new(Semaphore::new(1)))
    }

    fn try_acquire(&self) -> Result<OwnedSemaphorePermit, TryAcquireError> {
        self.0.clone().try_acquire_owned()
    }
}

#[derive(Clone)]
pub struct Context {
    //ctrl 必须由代理独有
    pub agent: Arc<RwLock<Agent>>,
    data_conns: Arc<RwLock<HashMap<String, Arc<Mutex<Channel>>>>>,
    next_data_conn: Arc<AtomicUsize>,
    data_x: (Sender<DataMessage>, SharedDataReceiver),
    command_gate: CommandGate,
}

//
pub struct Agent {
    pub config: Config,
    pub session_id: Option<String>,
    recv: mpsc::Receiver<CmdResp>,
    conn: Arc<Mutex<Channel>>,
}

impl Context {
    pub fn new(agent: Arc<RwLock<Agent>>) -> Self {
        let (tx, rx) = channel(DATA_QUEUE_CAPACITY);
        Context {
            agent,
            data_conns: Arc::new(RwLock::new(HashMap::new())),
            next_data_conn: Arc::new(AtomicUsize::new(0)),
            data_x: (tx, Arc::new(Mutex::new(rx))),
            // 服务端协议当前只允许一个活动命令。门禁覆盖完整命令生命周期，
            // 包括控制响应后的数据读取，避免并发 API 请求互相消费数据帧。
            command_gate: CommandGate::new(),
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
    pub fn get_data_rx(&self) -> Arc<Mutex<Receiver<(String, BytesMut)>>> {
        self.data_x.1.clone()
    }

    pub async fn enqueue_data(&self, message: DataMessage) -> anyhow::Result<()> {
        tokio::time::timeout(DATA_QUEUE_SEND_TIMEOUT, self.data_x.0.send(message))
            .await
            .map_err(|_| anyhow::anyhow!("控制端数据队列持续拥塞"))?
            .map_err(|_| anyhow::anyhow!("控制端数据接收任务已关闭"))
    }

    pub async fn send_data(&self, v: &[u8]) -> anyhow::Result<String> {
        let id = Uuid::new_v4().to_string();
        self.send_data_with_id(&id, v).await?;
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
        let receive = async {
            loop {
                match self.get_data_rx().lock().await.recv().await {
                    None => return Err(anyhow::Error::msg("数据通道已关闭")),
                    Some((id, data)) if id == data_id => return Ok(data),
                    Some(_) => continue,
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(6 * 60), receive)
            .await
            .map_err(|_| anyhow::anyhow!("等待数据响应超时"))?
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

    /// 控制帧成功写入后再放行关联数据任务。
    ///
    /// 这不是额外的网络握手，只是本进程内的时序门禁：控制连接写失败时，
    /// 上传任务不会提前把孤立分片塞入远端有界队列。
    pub async fn request_after_send(
        &self,
        cmd: &ReqCmd,
        transfer_start: Option<oneshot::Sender<()>>,
    ) -> anyhow::Result<CmdResp> {
        let request_result = self.agent.write().await.req(cmd, transfer_start).await;
        if let Ok(response) = request_result {
            return Ok(response);
        }

        // 控制命令可能已经被服务端执行。自动重放会让写文件或 Exec 等非幂等操作执行两次，
        // 因此这里只恢复后续请求所需的会话，并明确返回“结果未知”。
        if self.agent.write().await.re_conn(2).await.is_ok() {
            self.reset_data_connections().await;
            self.data_init()
                .await
                .map_err(|error| anyhow::anyhow!("控制连接已恢复，但数据通道恢复失败: {error}"))?;
            return Err(anyhow::anyhow!(
                "连接已恢复；为避免重复执行，本次命令未自动重放，请先查询状态再决定是否重试"
            ));
        }

        Err(anyhow::anyhow!("控制连接中断且自动恢复失败"))
    }

    async fn reset_data_connections(&self) {
        let old_connections = {
            let mut connections = self.data_conns.write().await;
            connections
                .drain()
                .map(|(_, channel)| channel)
                .collect::<Vec<_>>()
        };
        for channel in old_connections {
            channel.lock().await.try_write_half_close().await;
        }
    }
}

impl Agent {
    pub async fn create(config: &Config) -> anyhow::Result<Self> {
        let (conn, recv, session_id) = ctrl_conn(config).await?;
        Ok(Agent {
            config: config.clone(),
            session_id,
            conn,
            recv,
        })
    }
    pub async fn close(&mut self) {
        let _ = self.conn.clone().lock().await.write_half_close().await;
    }
    pub async fn re_conn(&mut self, retry_count: u32) -> anyhow::Result<()> {
        let mut last_error = None;
        for _ in 0..retry_count {
            match ctrl_conn(&self.config).await {
                Ok((conn, tx, session_id)) => {
                    self.conn = conn;
                    self.recv = tx;
                    self.session_id = session_id;
                    return Ok(());
                }
                Err(e) => {
                    last_error = Some(e);
                    time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            }
        }
        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("控制连接重试次数必须大于 0")))
    }

    pub async fn req(
        &mut self,
        cmd: &ReqCmd,
        transfer_start: Option<oneshot::Sender<()>>,
    ) -> anyhow::Result<CmdResp> {
        self.conn
            .lock()
            .await
            .write_and_flush(&ctrl_cmd_req(cmd.clone()))
            .await?;
        if transfer_start.is_some_and(|sender| sender.send(()).is_err()) {
            return Err(anyhow::Error::msg(
                "控制命令已写出，但关联数据发送任务已提前退出",
            ));
        }

        let response_timeout = if cmd.get_cmd_options().timeout() {
            CONTROL_RESPONSE_TIMEOUT
        } else {
            LONG_CONTROL_RESPONSE_TIMEOUT
        };
        tokio::time::timeout(response_timeout, async {
            for _ in 0..8 {
                let response = self
                    .recv
                    .recv()
                    .await
                    .ok_or_else(|| anyhow::anyhow!("控制响应通道已关闭"))?;
                if response.get_cmd_id() == cmd.get_id() {
                    return Ok(response);
                }
            }
            Err(anyhow::anyhow!("连续收到不匹配的控制响应"))
        })
        .await
        .map_err(|_| anyhow::anyhow!("等待控制响应超过命令总时限"))?
    }
}

pub fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::CommandGate;

    #[test]
    fn command_gate_rejects_parallel_command_until_permit_is_released() {
        let gate = CommandGate::new();
        let permit = gate.try_acquire().unwrap();
        assert!(gate.try_acquire().is_err());

        drop(permit);
        assert!(gate.try_acquire().is_ok());
    }
}
