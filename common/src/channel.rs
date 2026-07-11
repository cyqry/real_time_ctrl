use std::any::Any;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::net::tcp::OwnedWriteHalf;

use crate::secure_transport::BoxedAsyncWrite;

#[derive(Clone, Debug, PartialEq)]
pub enum ChannelType {
    Ctrl,
    CtrlData,
    Kik,
    KikData,
    Unknown,
}

pub struct Channel {
    pub channel_type: ChannelType,
    id: String,
    writer: BufWriter<BoxedAsyncWrite>,
    addr: (io::Result<SocketAddr>, io::Result<SocketAddr>),
    attr: HashMap<String, Box<dyn Any + Send + Sync>>,
    closed: AtomicBool,
    write_timeout: Duration,
}

impl Channel {
    pub fn new(
        writer: BoxedAsyncWrite,
        id: Option<String>,
        channel_type: ChannelType,
        local_addr: io::Result<SocketAddr>,
        peer_addr: io::Result<SocketAddr>,
    ) -> Self {
        Channel {
            id: id.unwrap_or("undefined_id".to_string()),
            channel_type,
            addr: (local_addr, peer_addr),
            writer: BufWriter::new(writer),
            attr: HashMap::new(),
            closed: AtomicBool::new(false),
            write_timeout: Duration::from_secs(45),
        }
    }

    pub fn from_tcp_writer(
        writer: OwnedWriteHalf,
        id: Option<String>,
        channel_type: ChannelType,
    ) -> Self {
        let local_addr = writer.local_addr();
        let peer_addr = writer.peer_addr();
        Self::new(Box::pin(writer), id, channel_type, local_addr, peer_addr)
    }

    pub fn get_local_addr(&self) -> &std::io::Result<SocketAddr> {
        &self.addr.0
    }
    pub fn get_peer_addr(&self) -> &std::io::Result<SocketAddr> {
        &self.addr.1
    }

    pub fn get_id(&self) -> &str {
        if self.id == "undefined_id" {
            panic!("未初始化的id被取")
        }
        self.id.as_str()
    }
    pub fn set_id(&mut self, id: String) {
        self.id = id;
    }

    pub fn set_write_timeout(&mut self, write_timeout: Duration) {
        self.write_timeout = write_timeout;
    }

    pub fn get_stream_info(&self) -> String {
        format!("local={:?}, peer={:?}", self.addr.0, self.addr.1)
    }

    pub fn put<T: 'static + Any + Send + Sync>(&mut self, key: String, value: T) {
        self.attr.insert(key, Box::new(value));
    }

    pub fn get<T: 'static + Any + Send + Sync>(&self, key: &str) -> Option<&T> {
        self.attr
            .get(key)
            .and_then(|value| value.downcast_ref::<T>())
    }
    pub fn get_mut<T: 'static + Any + Send + Sync>(&mut self, key: &str) -> Option<&mut T> {
        self.attr
            .get_mut(key)
            .and_then(|value| value.downcast_mut())
    }

    pub fn set<T: 'static + Any + Send + Sync>(
        &mut self,
        key: &str,
        mut f: impl FnMut(Option<&mut T>) -> anyhow::Result<T>,
    ) -> anyhow::Result<()> {
        let value = self
            .attr
            .get_mut(key)
            .and_then(|value| value.downcast_mut());
        if let Some(v) = value {
            let new_v = f(Some(v))?;
            *(v) = new_v;
        } else {
            let new_v = f(None)?;
            self.attr.insert(key.to_owned(), Box::new(new_v));
        }
        Ok(())
    }

    pub async fn write_half_close(&mut self) -> std::io::Result<()> {
        self.closed
            .store(true, std::sync::atomic::Ordering::Relaxed);
        tokio::time::timeout(self.write_timeout, self.writer.shutdown())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "关闭写半连接超时"))?
    }
    pub async fn try_write_half_close(&mut self) {
        let _ = self.writer.shutdown().await;
        self.closed
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
    pub async fn write_and_flush(&mut self, bys: &[u8]) -> anyhow::Result<()> {
        tokio::time::timeout(self.write_timeout, async {
            self.writer.write_all(bys).await?;
            self.writer.flush().await
        })
        .await
        .map_err(|_| anyhow::anyhow!("写连接超时"))??;
        Ok(())
    }
    pub async fn try_write_and_flush(&mut self, bys: &[u8]) {
        let _ = self.write_and_flush(bys).await;
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::Relaxed)
    }
}
