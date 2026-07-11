use std::any::Any;
use std::collections::HashMap;
use std::io;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::net::tcp::OwnedWriteHalf;

use crate::secure_transport::BoxedAsyncWrite;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelType {
    Ctrl,
    CtrlData,
    Kik,
    KikData,
    Unknown,
}

/// 带静态类型的连接属性键。
///
/// 连接初始化阶段需要暂存少量、与具体角色相关的状态。使用裸字符串配合 `Any`
/// 会让键名拼写和取值类型只能在运行时发现错误；该键把类型约束提前到编译期。
pub struct ChannelAttributeKey<T> {
    name: &'static str,
    marker: PhantomData<fn() -> T>,
}

impl<T> ChannelAttributeKey<T> {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            marker: PhantomData,
        }
    }
}

pub struct Channel {
    pub channel_type: ChannelType,
    id: Option<String>,
    writer: BufWriter<BoxedAsyncWrite>,
    addr: (io::Result<SocketAddr>, io::Result<SocketAddr>),
    attr: HashMap<String, Box<dyn Any + Send + Sync>>,
    closed: bool,
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
            id,
            channel_type,
            addr: (local_addr, peer_addr),
            writer: BufWriter::new(writer),
            attr: HashMap::new(),
            closed: false,
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

    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    pub fn require_id(&self) -> anyhow::Result<&str> {
        self.id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("连接 ID 尚未初始化"))
    }

    pub fn set_id(&mut self, id: String) {
        self.id = Some(id);
    }

    pub fn set_write_timeout(&mut self, write_timeout: Duration) {
        self.write_timeout = write_timeout;
    }

    pub fn get_stream_info(&self) -> String {
        format!("local={:?}, peer={:?}", self.addr.0, self.addr.1)
    }

    pub fn insert_attribute<T: 'static + Any + Send + Sync>(
        &mut self,
        key: &ChannelAttributeKey<T>,
        value: T,
    ) {
        self.attr.insert(key.name.to_string(), Box::new(value));
    }

    pub fn attribute<T: 'static + Any + Send + Sync>(
        &self,
        key: &ChannelAttributeKey<T>,
    ) -> Option<&T> {
        self.attr
            .get(key.name)
            .and_then(|value| value.downcast_ref::<T>())
    }

    pub async fn write_half_close(&mut self) -> std::io::Result<()> {
        self.closed = true;
        tokio::time::timeout(self.write_timeout, self.writer.shutdown())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "关闭写半连接超时"))?
    }
    pub async fn try_write_half_close(&mut self) {
        let _ = self.write_half_close().await;
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
        self.closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT_ATTRIBUTE: ChannelAttributeKey<String> =
        ChannelAttributeKey::new("test_text_attribute");

    fn channel(id: Option<String>) -> Channel {
        let (_peer, stream) = tokio::io::duplex(64);
        Channel::new(
            Box::pin(stream),
            id,
            ChannelType::Unknown,
            Err(io::Error::new(io::ErrorKind::NotConnected, "test")),
            Err(io::Error::new(io::ErrorKind::NotConnected, "test")),
        )
    }

    #[test]
    fn channel_id_is_explicitly_optional() {
        let mut channel = channel(None);
        assert!(channel.id().is_none());
        assert!(channel.require_id().is_err());

        channel.set_id("connection-1".to_string());
        assert_eq!(channel.id(), Some("connection-1"));
    }

    #[test]
    fn typed_attribute_round_trip() {
        let mut channel = channel(None);
        channel.insert_attribute(&TEXT_ATTRIBUTE, "value".to_string());
        assert_eq!(
            channel.attribute(&TEXT_ATTRIBUTE).map(String::as_str),
            Some("value")
        );
    }
}
