//! 控制端 TLS 传输的首包适配层：包装 socket，在连接生命周期内只发送一次协议前导。
//! 前导与首个 TLS 写入一起交给 socket，避免 Android 上两次独立 write 触发链路重置。
//! 这是 TCP 写入合并，不能承诺网络不会分包；TLS 协议和认证内容均保持不变。
use super::CTRL_TLS_PREFIX;
use std::io::{self, IoSlice};
use std::pin::Pin;
use std::task::{ready, Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

pub(super) struct PrefixedStream<S> {
    inner: S,
    // 只记已写出的前导字节数；短写入/Pending 后继续，不重复发前导，也不丢 TLS 字节。
    prefix_written: usize,
}

impl<S> PrefixedStream<S> {
    pub(super) fn new(inner: S) -> Self {
        Self {
            inner,
            prefix_written: 0,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buffer)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if buffer.is_empty() {
            return Poll::Ready(Ok(0));
        }
        while this.prefix_written < CTRL_TLS_PREFIX.len() {
            let prefix = &CTRL_TLS_PREFIX[this.prefix_written..];
            // TcpStream 支持 vectored write：两段数据使用一次系统调用发送，无堆分配。
            // 正常首包只有约 221 字节；之后直接透传，不为长期连接增加缓冲区。
            let buffers = [IoSlice::new(prefix), IoSlice::new(buffer)];
            let written = ready!(Pin::new(&mut this.inner).poll_write_vectored(cx, &buffers))?;
            if written == 0 {
                return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
            }
            let prefix_count = written.min(prefix.len());
            this.prefix_written += prefix_count;
            if written > prefix_count {
                // AsyncWrite 的返回值只能统计调用者的 TLS 字节，不能把内部前导算进去。
                return Poll::Ready(Ok(written - prefix_count));
            }
            // 若 socket 只写出部分前导，再尝试剩余内容。每次至少前进 1 字节，最多循环 5 次。
        }
        Pin::new(&mut this.inner).poll_write(cx, buffer)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffers: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        if self.prefix_written == CTRL_TLS_PREFIX.len() {
            // rustls 可把多个 TLS record 一起交给 socket。握手后必须保留这条快路径，
            // 否则 AsyncWrite 默认实现只写第一段，会增加 PC 大文件传输的系统调用次数。
            return Pin::new(&mut self.get_mut().inner).poll_write_vectored(cx, buffers);
        }
        // 首次只处理第一段非空 TLS 数据；返回的是已消费的业务字节数，调用者会继续
        // 提交剩余切片。首包合并和短写入状态仍统一由 poll_write 负责。
        match buffers.iter().find(|buffer| !buffer.is_empty()) {
            Some(buffer) => self.poll_write(cx, buffer),
            None => Poll::Ready(Ok(0)),
        }
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // flush 本身不提前发送前导，必须等到首个非空 TLS 写入才一起发。
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use tokio::io::AsyncWriteExt;

    enum Step {
        Bytes(usize),
        Pending,
    }
    struct SocketDouble {
        steps: VecDeque<Step>,
        calls: Vec<Vec<u8>>,
        bytes: Vec<u8>,
    }
    impl SocketDouble {
        fn new(steps: Vec<Step>) -> Self {
            Self {
                steps: steps.into(),
                calls: Vec::new(),
                bytes: Vec::new(),
            }
        }
        fn write(&mut self, cx: &mut Context<'_>, bytes: Vec<u8>) -> Poll<io::Result<usize>> {
            self.calls.push(bytes.clone());
            match self.steps.pop_front().unwrap_or(Step::Bytes(usize::MAX)) {
                Step::Pending => {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                Step::Bytes(limit) => {
                    let count = limit.min(bytes.len());
                    self.bytes.extend_from_slice(&bytes[..count]);
                    Poll::Ready(Ok(count))
                }
            }
        }
    }
    impl AsyncWrite for SocketDouble {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.get_mut().write(cx, bytes.to_vec())
        }
        fn poll_write_vectored(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buffers: &[IoSlice<'_>],
        ) -> Poll<io::Result<usize>> {
            self.get_mut().write(
                cx,
                buffers
                    .iter()
                    .flat_map(|buffer| buffer.iter().copied())
                    .collect(),
            )
        }
        fn is_write_vectored(&self) -> bool {
            true
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn first_write_combines_prefix_and_tls_without_buffering_later_writes() {
        let mut stream = PrefixedStream::new(SocketDouble::new(vec![]));
        stream.flush().await.unwrap();
        assert_eq!(stream.write(&[]).await.unwrap(), 0);
        assert!(stream.inner.calls.is_empty());
        assert_eq!(stream.write(b"client-hello").await.unwrap(), 12);
        stream.write_all(b"next-tls-record").await.unwrap();
        assert_eq!(
            stream.inner.calls,
            vec![
                [CTRL_TLS_PREFIX.as_slice(), b"client-hello"].concat(),
                b"next-tls-record".to_vec()
            ]
        );
    }

    #[tokio::test]
    async fn partial_prefix_pending_and_partial_tls_preserve_exact_byte_stream() {
        let mut stream = PrefixedStream::new(SocketDouble::new(vec![
            Step::Pending,
            Step::Bytes(2),
            Step::Pending,
            Step::Bytes(4),
            Step::Bytes(1),
        ]));
        stream.write_all(b"client-hello").await.unwrap();
        stream.write_all(b"next").await.unwrap();
        assert_eq!(
            stream.inner.bytes,
            [CTRL_TLS_PREFIX.as_slice(), b"client-hellonext"].concat()
        );
    }

    #[tokio::test]
    async fn vectored_tls_records_keep_socket_fast_path_after_prefix() {
        let mut stream = PrefixedStream::new(SocketDouble::new(vec![]));
        assert!(stream.is_write_vectored());
        assert_eq!(
            stream
                .write_vectored(&[IoSlice::new(&[]), IoSlice::new(b"hello")])
                .await
                .unwrap(),
            5
        );
        assert_eq!(
            stream
                .write_vectored(&[IoSlice::new(b"record-1"), IoSlice::new(b"record-2")])
                .await
                .unwrap(),
            16
        );
        assert_eq!(stream.inner.calls.len(), 2);
        assert_eq!(stream.inner.calls[1], b"record-1record-2");
        assert_eq!(
            stream.inner.bytes,
            [CTRL_TLS_PREFIX.as_slice(), b"hellorecord-1record-2"].concat()
        );
    }

    #[tokio::test]
    async fn zero_write_fails_instead_of_spinning_or_claiming_payload_was_sent() {
        let mut stream = PrefixedStream::new(SocketDouble::new(vec![Step::Bytes(0)]));
        assert_eq!(
            stream.write_all(b"hello").await.unwrap_err().kind(),
            io::ErrorKind::WriteZero
        );
        assert!(stream.inner.bytes.is_empty());
    }
}
