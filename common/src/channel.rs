//! 已建立连接的统一写端抽象。
//!
//! TLS、Noise 和普通测试流最终都被包装为 `Channel`。它保存连接角色、路由 ID、初始化属性以及
//! 带超时的写半连接；读取由各业务 crate 的 `FramedRead` 负责，因此这里没有读方法。

use std::any::Any;
use std::collections::HashMap;
use std::io;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedWriteHalf;

use crate::hidden;
use crate::secure_transport::BoxedAsyncWrite;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// 连接通过初始化消息认证后得到的角色。
///
/// `Unknown` 只允许出现在握手阶段；业务读循环必须在角色确定后切换到对应帧上限。
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
    id: u64,
    marker: PhantomData<fn() -> T>,
}

impl<T> ChannelAttributeKey<T> {
    /// 属性编号属于进程内协议，调用方应为每个语义分配固定且唯一的非零编号。
    pub const fn new(id: u64) -> Self {
        // const 上下文不能调用运行时解密宏；无消息断言避免把维护文本写入产物。
        assert!(id != 0);
        Self {
            id,
            marker: PhantomData,
        }
    }
}

/// 可在异步任务间共享的连接写端及其初始化状态。
///
/// 通常以 `Arc<Mutex<Channel>>` 持有。锁只保护短状态更新或一次有界网络写入，调用方不得在持锁时
/// 再获取全局会话表锁，以免形成跨连接死锁。
pub struct Channel {
    pub channel_type: ChannelType,
    id: Option<String>,
    writer: BoxedAsyncWrite,
    addr: (io::Result<SocketAddr>, io::Result<SocketAddr>),
    attr: HashMap<u64, Box<dyn Any + Send + Sync>>,
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
            // 每个协议帧都会主动 flush，额外套一层 BufWriter 既不能形成批量写，
            // 反而会让小帧多一次内存复制；TLS 自身已有记录层缓冲。
            writer,
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
            .ok_or_else(|| anyhow::Error::msg(hidden!("连接 ID 尚未初始化")))
    }

    pub fn set_id(&mut self, id: String) {
        self.id = Some(id);
    }

    pub fn set_write_timeout(&mut self, write_timeout: Duration) {
        self.write_timeout = write_timeout;
    }

    pub fn get_stream_info(&self) -> String {
        hidden!(
            "local=",
            crate::string_obfuscation::debug(&self.addr.0),
            ", peer=",
            crate::string_obfuscation::debug(&self.addr.1)
        )
    }

    pub fn insert_attribute<T: 'static + Any + Send + Sync>(
        &mut self,
        key: &ChannelAttributeKey<T>,
        value: T,
    ) {
        self.attr.insert(key.id, Box::new(value));
    }

    pub fn attribute<T: 'static + Any + Send + Sync>(
        &self,
        key: &ChannelAttributeKey<T>,
    ) -> Option<&T> {
        self.attr
            .get(&key.id)
            .and_then(|value| value.downcast_ref::<T>())
    }

    pub async fn write_half_close(&mut self) -> std::io::Result<()> {
        self.closed = true;
        tokio::time::timeout(self.write_timeout, self.writer.shutdown())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, hidden!("关闭写半连接超时")))?
    }
    pub async fn try_write_half_close(&mut self) {
        let _ = self.write_half_close().await;
    }
    pub async fn write_and_flush(&mut self, bys: &[u8]) -> anyhow::Result<()> {
        match tokio::time::timeout(self.write_timeout, async {
            self.writer.write_all(bys).await?;
            self.writer.flush().await
        })
        .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                // 写失败后 TCP 流边界已不可恢复，禁止后续分片继续复用该连接。
                self.closed = true;
                Err(error.into())
            }
            Err(_) => {
                // 超时会取消进行中的 write_all，连接里可能已有半帧，必须立即淘汰。
                self.closed = true;
                Err(anyhow::Error::msg(hidden!("写连接超时")))
            }
        }
    }
    pub fn is_closed(&self) -> bool {
        self.closed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT_ATTRIBUTE: ChannelAttributeKey<String> = ChannelAttributeKey::new(0x7465_7874);

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

    #[tokio::test]
    async fn timed_out_partial_write_marks_connection_closed() {
        let (_reader, writer) = tokio::io::duplex(1);
        let mut channel = Channel::new(
            Box::pin(writer),
            None,
            ChannelType::Unknown,
            Err(io::Error::new(io::ErrorKind::NotConnected, "test")),
            Err(io::Error::new(io::ErrorKind::NotConnected, "test")),
        );
        channel.set_write_timeout(Duration::from_millis(1));

        assert!(channel.write_and_flush(&[7_u8; 1024]).await.is_err());
        assert!(channel.is_closed());
    }
}
