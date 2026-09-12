use crate::hidden;
use crate::secure_transport::TransportParts;
use anyhow::Context;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use snow::params::NoiseParams;
use snow::{Builder, HandshakeState, StatelessTransportState};
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::timeout;

/// Kik 专用公网端口上的 Noise NK 协议标识。
/// 服务端不再在该端口兼容 TLS 或明文协议，完整前导不匹配即关闭连接。
pub const KIK_NOISE_PREFIX: [u8; 5] = [0x52, 0x54, 0x43, 0x4b, 0x03];

const NOISE_TAG_LENGTH: usize = 16;
const NOISE_MAX_MESSAGE_LENGTH: usize = u16::MAX as usize;
const NOISE_MAX_PLAINTEXT_LENGTH: usize = NOISE_MAX_MESSAGE_LENGTH - NOISE_TAG_LENGTH;
const HANDSHAKE_MAX_MESSAGE_LENGTH: usize = 1024;

#[derive(Clone)]
pub struct KikNoiseAcceptor {
    private_key: [u8; 32],
}

/// 服务端私钥只从运行环境进入内存；缺失时直接拒绝启动，禁止降级到明文。
pub fn build_kik_noise_acceptor(
    encoded_private_key: Option<&str>,
) -> anyhow::Result<KikNoiseAcceptor> {
    let encoded_private_key = encoded_private_key
        .ok_or_else(|| anyhow::Error::msg(hidden!("必须配置 CTRL_SERVER_KIK_NOISE_PRIVATE_KEY")))?;
    let private_key = decode_key(
        encoded_private_key,
        hidden!("CTRL_SERVER_KIK_NOISE_PRIVATE_KEY"),
    )?;
    Ok(KikNoiseAcceptor { private_key })
}

/// ctrl_kik 使用 Noise NK 建立匿名发起方连接。客户端不接收账号、证书、域名或机器信息，
/// 只用编译期 X25519 公钥校验服务端确实持有对应私钥，从而阻断主动中间人解密。
pub async fn connect_kik_noise(
    host: &str,
    port: &str,
    handshake_timeout: Duration,
    encoded_server_public_key: &str,
) -> anyhow::Result<TransportParts> {
    let server_public_key = decode_key(
        encoded_server_public_key,
        hidden!("KIK_NOISE_SERVER_PUBLIC_KEY"),
    )?;
    let endpoint = hidden!(host, ":", port);
    let stream = timeout(handshake_timeout, TcpStream::connect(&endpoint))
        .await
        .map_err(|_| anyhow::Error::msg(hidden!("连接 Kik 加密端口超时")))??;
    stream.set_nodelay(true)?;
    let local_addr = stream.local_addr();
    let peer_addr = stream.peer_addr();

    timeout(handshake_timeout, async move {
        let mut stream = stream;
        stream.write_all(&KIK_NOISE_PREFIX).await?;
        let mut state = Builder::new(noise_params()?)
            .remote_public_key(&server_public_key)?
            .prologue(noise_prologue())?
            .build_initiator()?;

        write_handshake_message(&mut stream, &mut state).await?;
        read_handshake_message(&mut stream, &mut state).await?;
        finish_transport(stream, state, local_addr, peer_addr)
    })
    .await
    .map_err(|_| anyhow::Error::msg(hidden!("Kik 加密握手超时")))?
}

pub async fn accept_kik_noise(
    acceptor: KikNoiseAcceptor,
    stream: TcpStream,
    handshake_timeout: Duration,
) -> anyhow::Result<TransportParts> {
    let local_addr = stream.local_addr();
    let peer_addr = stream.peer_addr();
    timeout(handshake_timeout, async move {
        let mut stream = stream;
        let mut prefix = [0_u8; KIK_NOISE_PREFIX.len()];
        stream.read_exact(&mut prefix).await?;
        if prefix != KIK_NOISE_PREFIX {
            return Err(anyhow::Error::msg(hidden!("Kik 加密协议前缀无效")));
        }

        let mut state = Builder::new(noise_params()?)
            .local_private_key(&acceptor.private_key)?
            .prologue(noise_prologue())?
            .build_responder()?;
        read_handshake_message(&mut stream, &mut state).await?;
        write_handshake_message(&mut stream, &mut state).await?;
        finish_transport(stream, state, local_addr, peer_addr)
    })
    .await
    .map_err(|_| anyhow::Error::msg(hidden!("Kik 加密握手超时")))?
}

fn noise_params() -> anyhow::Result<NoiseParams> {
    hidden!("Noise_NK_25519_ChaChaPoly_BLAKE2s")
        .parse()
        .context(hidden!("Noise 协议参数无效"))
}

fn noise_prologue() -> &'static [u8] {
    // 协议域分隔符用字节常量表达，避免在受保护客户端中留下可搜索的实现标签。
    &[
        0x72, 0x74, 0x63, 0x2d, 0x6b, 0x69, 0x6b, 0x2d, 0x74, 0x72, 0x61, 0x6e, 0x73, 0x70, 0x6f,
        0x72, 0x74, 0x2d, 0x76, 0x33,
    ]
}

fn decode_key(value: &str, field: String) -> anyhow::Result<[u8; 32]> {
    let value = value.trim();
    let decoded = STANDARD
        .decode(value)
        .or_else(|_| URL_SAFE_NO_PAD.decode(value))
        .or_else(|_| hex::decode(value))
        .with_context(|| hidden!(field, " 必须是 Base64、Base64URL 或十六进制密钥"))?;
    decoded.try_into().map_err(|bytes: Vec<u8>| {
        anyhow::Error::msg(hidden!(
            field,
            " 解码后必须为 32 字节，实际为 ",
            bytes.len()
        ))
    })
}

async fn write_handshake_message(
    stream: &mut TcpStream,
    state: &mut HandshakeState,
) -> anyhow::Result<()> {
    let mut message = [0_u8; HANDSHAKE_MAX_MESSAGE_LENGTH];
    let length = state.write_message(&[], &mut message)?;
    let length = u16::try_from(length).context(hidden!("Noise 握手消息过长"))?;
    stream.write_all(&length.to_be_bytes()).await?;
    stream.write_all(&message[..usize::from(length)]).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_handshake_message(
    stream: &mut TcpStream,
    state: &mut HandshakeState,
) -> anyhow::Result<()> {
    let length = stream.read_u16().await? as usize;
    if length == 0 || length > HANDSHAKE_MAX_MESSAGE_LENGTH {
        return Err(anyhow::Error::msg(hidden!("Noise 握手消息长度无效")));
    }
    let mut message = vec![0_u8; length];
    stream.read_exact(&mut message).await?;
    let mut payload = [0_u8; HANDSHAKE_MAX_MESSAGE_LENGTH];
    state.read_message(&message, &mut payload)?;
    Ok(())
}

fn finish_transport(
    stream: TcpStream,
    state: HandshakeState,
    local_addr: io::Result<SocketAddr>,
    peer_addr: io::Result<SocketAddr>,
) -> anyhow::Result<TransportParts> {
    if !state.is_handshake_finished() {
        return Err(anyhow::Error::msg(hidden!("Noise 握手未完成")));
    }
    let transport = Arc::new(state.into_stateless_transport_mode()?);
    let (reader, writer) = stream.into_split();
    Ok(TransportParts {
        reader: Box::pin(NoiseReader::new(reader, transport.clone())),
        writer: Box::pin(NoiseWriter::new(writer, transport)),
        local_addr,
        peer_addr,
    })
}

struct NoiseReader<R> {
    reader: R,
    state: Arc<StatelessTransportState>,
    nonce: u64,
    header: [u8; 2],
    header_read: usize,
    ciphertext: Vec<u8>,
    ciphertext_read: usize,
    plaintext: Vec<u8>,
    plaintext_offset: usize,
}

impl<R> NoiseReader<R> {
    fn new(reader: R, state: Arc<StatelessTransportState>) -> Self {
        Self {
            reader,
            state,
            nonce: 0,
            header: [0; 2],
            header_read: 0,
            ciphertext: Vec::with_capacity(NOISE_MAX_MESSAGE_LENGTH),
            ciphertext_read: 0,
            plaintext: Vec::with_capacity(NOISE_MAX_PLAINTEXT_LENGTH),
            plaintext_offset: 0,
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for NoiseReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if this.plaintext_offset < this.plaintext.len() {
                let available = &this.plaintext[this.plaintext_offset..];
                let copy_length = available.len().min(output.remaining());
                output.put_slice(&available[..copy_length]);
                this.plaintext_offset += copy_length;
                if this.plaintext_offset == this.plaintext.len() {
                    this.plaintext.clear();
                    this.plaintext_offset = 0;
                }
                return Poll::Ready(Ok(()));
            }

            while this.header_read < this.header.len() {
                let mut buffer = ReadBuf::new(&mut this.header[this.header_read..]);
                match Pin::new(&mut this.reader).poll_read(cx, &mut buffer) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Ready(Ok(())) if buffer.filled().is_empty() => {
                        if this.header_read == 0 {
                            return Poll::Ready(Ok(()));
                        }
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            hidden!("Noise 记录头不完整"),
                        )));
                    }
                    Poll::Ready(Ok(())) => this.header_read += buffer.filled().len(),
                }
            }

            if this.ciphertext.is_empty() {
                let length = u16::from_be_bytes(this.header) as usize;
                if !(NOISE_TAG_LENGTH..=NOISE_MAX_MESSAGE_LENGTH).contains(&length) {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        hidden!("Noise 记录长度无效"),
                    )));
                }
                this.ciphertext.resize(length, 0);
            }

            while this.ciphertext_read < this.ciphertext.len() {
                let mut buffer = ReadBuf::new(&mut this.ciphertext[this.ciphertext_read..]);
                match Pin::new(&mut this.reader).poll_read(cx, &mut buffer) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Ready(Ok(())) if buffer.filled().is_empty() => {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            hidden!("Noise 记录正文不完整"),
                        )));
                    }
                    Poll::Ready(Ok(())) => this.ciphertext_read += buffer.filled().len(),
                }
            }

            this.plaintext.resize(this.ciphertext.len(), 0);
            let plaintext_length = this
                .state
                .read_message(this.nonce, &this.ciphertext, &mut this.plaintext)
                .map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, hidden!("Noise 记录认证失败"))
                })?;
            this.nonce = this.nonce.checked_add(1).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, hidden!("Noise 接收 nonce 耗尽"))
            })?;
            this.plaintext.truncate(plaintext_length);
            this.header_read = 0;
            this.ciphertext.clear();
            this.ciphertext_read = 0;
        }
    }
}

struct NoiseWriter<W> {
    writer: W,
    state: Arc<StatelessTransportState>,
    nonce: u64,
    pending: Vec<u8>,
    pending_offset: usize,
}

impl<W> NoiseWriter<W> {
    fn new(writer: W, state: Arc<StatelessTransportState>) -> Self {
        Self {
            writer,
            state,
            nonce: 0,
            pending: Vec::with_capacity(NOISE_MAX_MESSAGE_LENGTH + 2),
            pending_offset: 0,
        }
    }
}

impl<W: AsyncWrite + Unpin> NoiseWriter<W> {
    fn poll_pending(&mut self, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        while self.pending_offset < self.pending.len() {
            match Pin::new(&mut self.writer).poll_write(cx, &self.pending[self.pending_offset..]) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        hidden!("Noise 记录写入返回零字节"),
                    )));
                }
                Poll::Ready(Ok(written)) => self.pending_offset += written,
            }
        }
        self.pending.clear();
        self.pending_offset = 0;
        Poll::Ready(Ok(()))
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for NoiseWriter<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        input: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if !this.pending.is_empty() {
            match this.poll_pending(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) => {}
            }
        }
        if input.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let input_length = input.len().min(NOISE_MAX_PLAINTEXT_LENGTH);
        this.pending.resize(input_length + NOISE_TAG_LENGTH + 2, 0);
        let encrypted_length = this
            .state
            .write_message(this.nonce, &input[..input_length], &mut this.pending[2..])
            .map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, hidden!("Noise 记录加密失败"))
            })?;
        let encrypted_length_u16 = u16::try_from(encrypted_length)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, hidden!("Noise 记录过长")))?;
        this.pending[..2].copy_from_slice(&encrypted_length_u16.to_be_bytes());
        this.pending.truncate(encrypted_length + 2);
        this.nonce = this.nonce.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, hidden!("Noise 发送 nonce 耗尽"))
        })?;

        // 数据已复制进受控的加密记录缓冲，可以向 write_all 报告消费；后续 poll_write/flush
        // 会先排空该记录，保证 TCP 上严格有序且内存始终只有一个约 64 KiB 的待写记录。
        Poll::Ready(Ok(input_length))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.poll_pending(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => Pin::new(&mut this.writer).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match self.as_mut().poll_flush(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Ready(Ok(())) => Pin::new(&mut self.get_mut().writer).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn noise_transport_round_trips_multiple_records_bidirectionally() {
        let params = noise_params().unwrap();
        let keypair = Builder::new(params).generate_keypair().unwrap();
        let private_key: [u8; 32] = keypair.private.try_into().unwrap();
        let public_key = STANDARD.encode(keypair.public);
        let listener = TcpListener::bind(hidden!("127.0.0.1", ":", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            accept_kik_noise(
                KikNoiseAcceptor { private_key },
                stream,
                Duration::from_secs(5),
            )
            .await
            .unwrap()
        });
        let mut client = connect_kik_noise(
            "127.0.0.1",
            &address.port().to_string(),
            Duration::from_secs(5),
            &public_key,
        )
        .await
        .unwrap();
        let mut server = server.await.unwrap();

        let request = vec![0x5a; NOISE_MAX_PLAINTEXT_LENGTH * 2 + 17];
        client.writer.write_all(&request).await.unwrap();
        client.writer.flush().await.unwrap();
        let mut received = vec![0; request.len()];
        server.reader.read_exact(&mut received).await.unwrap();
        assert_eq!(received, request);

        let response = vec![0xa5; NOISE_MAX_PLAINTEXT_LENGTH + 9];
        server.writer.write_all(&response).await.unwrap();
        server.writer.flush().await.unwrap();
        let mut received = vec![0; response.len()];
        client.reader.read_exact(&mut received).await.unwrap();
        assert_eq!(received, response);
    }

    #[test]
    fn rejects_invalid_key_length() {
        assert!(decode_key("AQID", hidden!("test-key")).is_err());
    }
}
