use crate::ctrl_conn::ctrl_conn;
use crate::ctrl_data_conn::ctrl_data_conn;
use bytes::{BufMut, BytesMut};
use common::channel::Channel;
use common::config::Config;
use common::protocol;
use common::protocol::ReqCmd;
use ctrl_common::ctrl_frame::Frame;
use ctrl_common::ctrl_protocol::ctrl_cmd_req;
use ctrl_common::ctrl_resp::CmdResp;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::time;
use uuid::Uuid;

type DataMessage = (String, BytesMut);
type SharedDataReceiver = Arc<Mutex<Receiver<DataMessage>>>;

#[derive(Clone)]
pub struct Context {
    //ctrl 必须由代理独有
    pub agent: Arc<RwLock<Agent>>,
    data_conns: Arc<RwLock<HashMap<String, Arc<Mutex<Channel>>>>>,
    next_data_conn: Arc<AtomicUsize>,
    data_x: (Sender<DataMessage>, SharedDataReceiver),
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
        let (tx, rx) = channel(5);
        Context {
            agent,
            data_conns: Arc::new(RwLock::new(HashMap::new())),
            next_data_conn: Arc::new(AtomicUsize::new(0)),
            data_x: (tx, Arc::new(Mutex::new(rx))),
        }
    }
    pub async fn insert_ctrl_data_conn(&self, data_conn: Arc<Mutex<Channel>>) {
        let id = data_conn.lock().await.get_id().to_string();
        self.data_conns.write().await.insert(id, data_conn);
    }
    pub async fn delete_ctrl_data_conn(&self, data_conn: Arc<Mutex<Channel>>) {
        let id = data_conn.clone().lock().await.get_id().to_string();
        self.data_conns.write().await.remove(id.as_str());
    }
    pub fn get_data_tx(&self) -> Sender<(String, BytesMut)> {
        self.data_x.0.clone()
    }

    pub fn get_data_rx(&self) -> Arc<Mutex<Receiver<(String, BytesMut)>>> {
        self.data_x.1.clone()
    }

    pub async fn send_data(&self, v: &[u8]) -> anyhow::Result<String> {
        let id = Uuid::new_v4().to_string();
        self.send_data_with_id(id.clone(), v).await?;
        Ok(id)
    }

    pub async fn send_data_with_id(&self, data_id: String, v: &[u8]) -> anyhow::Result<()> {
        match self.find_ctrl_data().await {
            None => Err(anyhow::Error::msg("应用数据传输通道未初始化!")),
            Some(data_conn) => {
                let mut bytes_mut = BytesMut::with_capacity(v.len());
                bytes_mut.put_slice(v);

                let mut guard = data_conn.lock().await;
                guard
                    .write_and_flush(&protocol::transfer_encode_frame(Frame::Data(
                        data_id, bytes_mut,
                    )))
                    .await?;
                Ok(())
            }
        }
    }

    pub async fn wait_data(&self, data_id: &str) -> anyhow::Result<Vec<u8>> {
        let receive = async {
            for _ in 0..8 {
                match self.get_data_rx().lock().await.recv().await {
                    None => return Err(anyhow::Error::msg("数据通道已关闭")),
                    Some((id, data)) if id == data_id => return Ok(data.to_vec()),
                    Some(_) => continue,
                }
            }
            Err(anyhow::Error::msg("连续收到不匹配的数据帧"))
        };
        tokio::time::timeout(Duration::from_secs(6 * 60), receive)
            .await
            .map_err(|_| anyhow::anyhow!("等待数据响应超时"))?
    }

    pub async fn find_ctrl_data(&self) -> Option<Arc<Mutex<Channel>>> {
        let next_arc = self.next_data_conn.clone();
        let arc = self.data_conns.clone();
        let data_map = arc.read().await;
        if data_map.is_empty() {
            return None;
        }
        let next = next_arc.fetch_add(1, Ordering::Relaxed) % data_map.len();
        let c = data_map.values().nth(next).cloned()?;
        Some(c)
    }
    pub async fn data_init(&self) -> anyhow::Result<()> {
        let config = self.agent.read().await.config.clone();
        ctrl_data_conn(self.clone(), &config).await
    }

    pub async fn request(&self, cmd: &ReqCmd) -> anyhow::Result<CmdResp> {
        let request_result = self.agent.write().await.req(cmd).await;
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
        let mut re = anyhow::Error::msg("unreachable!");
        for _ in 0..retry_count {
            match ctrl_conn(&self.config).await {
                Ok((conn, tx, session_id)) => {
                    self.conn = conn;
                    self.recv = tx;
                    self.session_id = session_id;
                    return Ok(());
                }
                Err(e) => {
                    re = e;
                    time::sleep(Duration::from_secs(2)).await;
                    continue;
                }
            }
        }
        Err(re)
    }

    pub async fn req(&mut self, cmd: &ReqCmd) -> anyhow::Result<CmdResp> {
        self.conn
            .lock()
            .await
            .write_and_flush(&ctrl_cmd_req(cmd.clone()))
            .await?;

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
    }
}

pub fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
