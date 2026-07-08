
use std::any::Any;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;
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
    create_time: SystemTime,
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
            create_time: SystemTime::now(),
            closed: AtomicBool::new(false),
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

    pub fn get_stream_info(&self) -> String {
        format!("local={:?}, peer={:?}", self.addr.0, self.addr.1)
    }

    pub fn put<T: 'static + Any + Send + Sync>(&mut self, key: String, value: T) {
        self.attr.insert(key, Box::new(value));
    }

    pub fn get<T: 'static + Any + Send + Sync>(&self, key: &str) -> Option<&T> {
        let option = self.attr.get(key);
        let option1 = option.and_then(|value| value.downcast_ref::<T>());
        option1.and_then(|v| Some(v))
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
        if value.is_some() {
            let v = value.unwrap();
            let new_v = f(Some(v))?;
            *(v) = new_v;
        } else {
            let new_v = f(value)?;
            self.attr.insert(key.to_owned(), Box::new(new_v));
        }
        Ok(())
    }

    pub async fn write_half_close(&mut self) -> std::io::Result<()> {
        self.closed.store(true, std::sync::atomic::Ordering::Relaxed);
        self.writer.shutdown().await
    }
    pub async fn try_write_half_close(&mut self) {
        match self.writer.shutdown().await {
            Ok(_) => {}
            Err(_) => {}
        };
        self.closed.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    pub async fn write_and_flush(&mut self, bys: &[u8]) -> anyhow::Result<()> {
        
        let w = self.writer.write_all(bys).await?;
        self.writer.flush().await?;
        Ok(w)
    }
    pub async fn try_write_and_flush(&mut self, bys: &[u8]) {
        match self.writer.write_all(bys).await {
            Ok(_) => {}
            Err(_) => {
                return;
            }
        };
        match self.writer.flush().await {
            Ok(_) => {}
            Err(_) => {}
        }
    }
    
    pub fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::Relaxed)
    }
}
