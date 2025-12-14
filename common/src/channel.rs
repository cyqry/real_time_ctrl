use crate::ltc_codec::LengthFieldBasedFrameDecoder;
use crate::message::frame::Frame;
use anyhow::Error;
use bytes::BytesMut;
use std::any::Any;
use std::collections::HashMap;
use std::fmt::Debug;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, BufWriter, ReadBuf};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::TcpStream;
use tokio::time;
use tokio::time::Instant;
use tokio_util::codec::Framed;

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
    writer: BufWriter<OwnedWriteHalf>,
    addr: (io::Result<SocketAddr>, io::Result<SocketAddr>),
    attr: HashMap<String, Box<dyn Any + Send + Sync>>,
    create_time: Instant,
}

impl Channel {
    pub fn new(writer: OwnedWriteHalf, id: Option<String>, channel_type: ChannelType) -> Self {
        
        Channel {
            id: id.unwrap_or("undefined_id".to_string()),
            channel_type,
            addr: (writer.local_addr(),writer.peer_addr()),
            writer: BufWriter::new(writer),
            attr: HashMap::new(),
            create_time: time::Instant::now(),
        }
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
        format!("{:?}", self.writer)
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
        self.writer.shutdown().await
    }
    pub async fn try_write_half_close(&mut self) {
        match self.writer.shutdown().await {
            Ok(_) => {}
            Err(_) => {}
        };
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
}
