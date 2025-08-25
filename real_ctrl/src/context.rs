use crate::ctrl_conn::ctrl_conn;
use crate::ctrl_data_conn::ctrl_data_conn;
use bytes::{Buf, BufMut, BytesMut};
use common::channel::Channel;
use common::command::Command;
use common::config::Config;
use common::message::resp::Resp;
use common::protocol;
use common::protocol::BufSerializable;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI16, AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::time;
use tokio::time::timeout;

#[derive(Clone)]
pub struct Context {
    //ctrl 必须由代理独有
    pub agent: Arc<RwLock<Agent>>,
    data_conns: Arc<RwLock<HashMap<String, Arc<Mutex<Channel>>>>>,
    next_data_conn: Arc<AtomicU16>,
    data_x: (
        Sender<(String, BytesMut)>,
        Arc<Mutex<Receiver<(String, BytesMut)>>>,
    ),
}

//
pub struct Agent {
    pub config: Config,
    recv: mpsc::Receiver<Resp>,
    conn: Arc<Mutex<Channel>>,
}

impl Context {
    pub fn new(agent: Arc<RwLock<Agent>>) -> Self {
        let (tx, rx) = channel(5);
        Context {
            agent,
            data_conns: Arc::new(RwLock::new(HashMap::new())),
            next_data_conn: Arc::new(AtomicU16::new(0)),
            data_x: (tx, Arc::new(Mutex::new(rx))),
        }
    }
    pub async fn insert_ctrl_data_conn(&self, data_conn: Arc<Mutex<Channel>>) {
        let id = data_conn.clone().lock().await.get_id().to_string();
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

    pub async fn wait_data(&self, data_id: &str) -> anyhow::Result<Vec<u8>> {
        for _ in 0..3 {
            //害怕有大文件，所以不设置超时时间
            match self.get_data_rx().lock().await.recv().await {
                None => {
                    unreachable!("unreachable");
                }
                Some((id, data)) => {
                    if id == data_id {
                        return Ok(data.to_vec());
                    } else {
                        //有过期数据，重试三次
                        continue;
                    }
                }
            }
        }
        Err(anyhow::Error::msg("获取到的文件有错误!"))
    }

    pub async fn find_ctrl_data(&self) -> Option<Arc<Mutex<Channel>>> {
        let next_arc = self.next_data_conn.clone();
        let arc = self.data_conns.clone();
        let data_map = arc.read().await;
        if data_map.len() == 0 {
            return None;
        }
        let next = (next_arc.load(Ordering::SeqCst) + 1) % data_map.len() as u16;
        let c = data_map.values().nth(next as usize).unwrap().clone();
        next_arc.store(next + 1, Ordering::SeqCst);
        Some(c)
    }
    pub async fn data_init(&self) -> anyhow::Result<()> {
        let config = self.agent.clone().read().await.config.clone();
        ctrl_data_conn(self.clone(), &config).await
    }
}

impl Agent {
    pub async fn create(config: &Config) -> anyhow::Result<Self> {
        let (conn, recv) = ctrl_conn(config).await?;
        Ok(Agent {
            config: config.clone(),
            conn,
            recv,
        })
    }
    pub async fn close(&mut self) {
        match self.conn.clone().lock().await.write_half_close().await {
            Ok(_) => {}
            Err(_) => {}
        }
    }
    pub async fn re_conn(&mut self, count: u32) -> anyhow::Result<()> {
        let mut re = anyhow::Error::msg("unreachable!");
        for _ in 0..count {
            match ctrl_conn(&self.config).await {
                Ok((conn, tx)) => {
                    self.conn = conn;
                    self.recv = tx;
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

    pub async fn req(&mut self, cmd: &Command) -> anyhow::Result<Resp> {
        let mut re = anyhow::Error::msg("unreachable!");
        //由于编译器无法确定这个for是否至少有一次循环，所以需要re变量初始化
        for _ in 0..3 {
            //write
            match self
                .conn
                .clone()
                .lock()
                .await
                .write_and_flush(&protocol::cmd(cmd.clone()))
                .await
            {
                Ok(_) => {}
                Err(e) => {
                    re = e;
                    self.re_conn(2).await?;
                    continue;
                }
            };
            //read
            match self.recv.recv().await {
                None => {
                    re = anyhow::Error::msg("无法读");
                    self.re_conn(2).await?;
                    continue;
                }
                Some(resp) => {
                    return Ok(resp);
                }
            }
        }
        Err(re)
    }
}
