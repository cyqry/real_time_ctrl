//! 本机 E2E 的任务数据归属探针：通过真实 Noise 握手验证错误证明、挑战跨连接和重放拒绝。
//! 最后占用真实 Kik 的第四条未绑定数据连接，父脚本执行任务时验证该连接收不到程序数据。
//! 只接受 127.0.0.1，握手、读取和整个探针均有超时；不读取服务端私钥或控制账号秘密。

use anyhow::{anyhow, bail, ensure, Context, Result};
use bytes::BytesMut;
use common::{
    kik_info::KikInfo,
    ltc_codec::{LengthFieldBasedFrameDecoder, DATA_MAX_FRAME_LENGTH},
    message::{init_frame::InitFrame, kik_frame::KikFrame},
    noise_transport::connect_kik_noise,
    protocol::{self, BufSerializable},
    secure_transport::{BoxedAsyncRead, BoxedAsyncWrite},
    task::{self, TaskFrame},
};
use std::{env, path::PathBuf, time::Duration};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    time::{timeout, Instant},
};
use tokio_stream::StreamExt;
use tokio_util::codec::FramedRead;

struct Client {
    reader: FramedRead<BufReader<BoxedAsyncRead>, LengthFieldBasedFrameDecoder>,
    writer: BoxedAsyncWrite,
}

struct Endpoint {
    port: String,
    public_key: String,
}

impl Client {
    async fn connect(endpoint: &Endpoint) -> Result<Self> {
        let parts = connect_kik_noise(
            "127.0.0.1",
            &endpoint.port,
            Duration::from_secs(10),
            &endpoint.public_key,
        )
        .await
        .context("establish local Noise NK connection")?;
        Ok(Self {
            reader: FramedRead::new(
                BufReader::new(parts.reader),
                LengthFieldBasedFrameDecoder::new_with_max_frame_len(DATA_MAX_FRAME_LENGTH),
            ),
            writer: parts.writer,
        })
    }

    async fn send(&mut self, frame: impl BufSerializable) -> Result<()> {
        let encoded = protocol::transfer_encode_frame(frame);
        // NoiseWriter 接收明文只代表最后一个加密记录进入 pending；flush 后才真正发到 TCP。
        // 写入和排空共享同一个预算，不能先等响应、靠下一帧写入偶然把上一帧发送出去。
        timeout(Duration::from_secs(10), async {
            self.writer.write_all(&encoded).await?;
            self.writer.flush().await
        })
        .await
        .context("write and flush Noise application frame deadline")?
        .context("write and flush Noise application frame")?;
        Ok(())
    }

    async fn next(&mut self) -> Result<BytesMut> {
        timeout(Duration::from_secs(10), self.reader.next())
            .await
            .context("read Noise application frame deadline")?
            .ok_or_else(|| anyhow!("probe connection closed"))?
            .map_err(Into::into)
    }

    async fn task(&mut self) -> Result<TaskFrame> {
        loop {
            match KikFrame::from_buf(self.next().await?) {
                Some(KikFrame::Task(frame)) => return Ok(frame),
                Some(KikFrame::Ping | KikFrame::Pong) => self.send(KikFrame::Pong).await?,
                _ => bail!("unexpected frame in task binding handshake"),
            }
        }
    }

    async fn main(
        endpoint: &Endpoint,
        id: Option<String>,
        key: [u8; 32],
    ) -> Result<(Self, String)> {
        let mut client = Self::connect(endpoint).await?;
        client
            .send(InitFrame::KikReq(KikInfo {
                id,
                name: "task-binding-e2e-probe".into(),
            }))
            .await
            .context("send probe Kik main registration")?;
        let id = match InitFrame::from_buf(client.next().await.context("read probe main KikId")?) {
            Some(InitFrame::KikId(id)) => id,
            _ => bail!("probe main registration failed"),
        };
        client
            .send(KikFrame::Task(TaskFrame::Hello(key)))
            .await
            .context("send probe task Hello")?;
        ensure!(
            matches!(
                client.task().await.context("read probe task HelloAck")?,
                TaskFrame::HelloAck
            ),
            "missing HelloAck"
        );
        Ok((client, id))
    }

    async fn data(endpoint: &Endpoint, id: &str) -> Result<Self> {
        let mut client = Self::connect(endpoint).await?;
        client
            .send(InitFrame::KikDataConnReq(id.into()))
            .await
            .context("send probe Kik data registration")?;
        ensure!(
            matches!(InitFrame::from_buf(client.next().await.context("read probe data KikId")?), Some(InitFrame::KikId(actual)) if actual == id),
            "probe data registration failed"
        );
        Ok(client)
    }

    async fn challenge(&mut self) -> Result<[u8; 32]> {
        self.send(KikFrame::Task(TaskFrame::BindRequest))
            .await
            .context("send task data BindRequest")?;
        match self.task().await.context("read task data Challenge")? {
            TaskFrame::Challenge(nonce) => Ok(nonce),
            _ => bail!("missing data binding challenge"),
        }
    }

    /// 错误证明必须导致 EOF/复位，而不是保持连接至探针超时。
    async fn rejected(&mut self) -> Result<()> {
        timeout(Duration::from_secs(8), async {
            loop {
                match self.reader.next().await {
                    None | Some(Err(_)) => return Ok(()),
                    Some(Ok(bytes)) => match KikFrame::from_buf(bytes) {
                        Some(KikFrame::Ping | KikFrame::Pong) => self.send(KikFrame::Pong).await?,
                        _ => bail!("invalid proof received a business/binding response"),
                    },
                }
            }
        })
        .await
        .context("server did not reject the invalid proof within the deadline")?
    }

    async fn close(mut self) {
        let _ = timeout(Duration::from_secs(2), self.writer.shutdown()).await;
    }
}

async fn negative_proofs(endpoint: &Endpoint) -> Result<Vec<&'static str>> {
    let key = task::random_key();
    let (main, id) = Client::main(endpoint, None, key)
        .await
        .context("negative proofs: register initial main")?;
    let mut assertions = Vec::new();

    let mut wrong = Client::data(endpoint, &id)
        .await
        .context("incorrect key: data connection")?;
    let nonce = wrong
        .challenge()
        .await
        .context("incorrect key: challenge")?;
    wrong
        .send(KikFrame::Task(TaskFrame::Proof(task::binding_proof(
            &task::random_key(),
            &nonce,
        ))))
        .await
        .context("incorrect key: send proof")?;
    wrong
        .rejected()
        .await
        .context("incorrect key: require rejection")?;
    drop(wrong);
    assertions.push("real Noise data connection rejects an incorrect binding key");

    let mut first = Client::data(endpoint, &id)
        .await
        .context("cross connection: first registration")?;
    let mut second = Client::data(endpoint, &id)
        .await
        .context("cross connection: second registration")?;
    let first_nonce = first
        .challenge()
        .await
        .context("cross connection: first challenge")?;
    let _ = second
        .challenge()
        .await
        .context("cross connection: second challenge")?;
    let proof = task::binding_proof(&key, &first_nonce);
    second
        .send(KikFrame::Task(TaskFrame::Proof(proof)))
        .await
        .context("cross connection: send foreign proof")?;
    second
        .rejected()
        .await
        .context("cross connection: require rejection")?;
    drop(second);
    assertions.push("real Noise data connection rejects another connection's challenge proof");

    first
        .send(KikFrame::Task(TaskFrame::Proof(proof)))
        .await
        .context("valid binding: send original proof")?;
    ensure!(
        matches!(
            first.task().await.context("valid binding: read Bound")?,
            TaskFrame::Bound
        ),
        "valid binding was rejected"
    );
    first
        .send(KikFrame::Task(TaskFrame::Proof(proof)))
        .await
        .context("replay: send previously accepted proof")?;
    first
        .rejected()
        .await
        .context("replay: require rejection")?;
    drop(first);
    assertions.push("real Noise data connection accepts its proof once and rejects replay");

    let mut stale = Client::data(endpoint, &id)
        .await
        .context("main replacement: data registration")?;
    let old_nonce = stale
        .challenge()
        .await
        .context("main replacement: original challenge")?;
    let old_proof = task::binding_proof(&key, &old_nonce);
    main.close().await;
    let (replacement, _) = Client::main(endpoint, Some(id), task::random_key())
        .await
        .context("main replacement: register new main generation")?;
    if stale
        .send(KikFrame::Task(TaskFrame::Proof(old_proof)))
        .await
        .is_ok()
    {
        stale
            .rejected()
            .await
            .context("main replacement: require old proof rejection")?;
    }
    drop(stale);
    replacement.close().await;
    assertions.push("main reconnection invalidates a previously issued data challenge");
    Ok(assertions)
}

async fn probe() -> Result<()> {
    let port = env::var("RTC_E2E_TASK_NOISE_PORT")?;
    ensure!(port.parse::<u16>()? > 0, "invalid local port");
    let endpoint = Endpoint {
        port,
        public_key: env::var("RTC_E2E_TASK_NOISE_PUBLIC_KEY").unwrap_or_else(|_| {
            common::generated::encrypted_strings::KIK_NOISE_SERVER_PUBLIC_KEY()
        }),
    };
    let target = env::var("RTC_E2E_TASK_KIK_ID")?;
    uuid::Uuid::parse_str(&target)?;
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("target")
        .canonicalize()?;
    let markers = PathBuf::from(env::var("RTC_E2E_TASK_PROBE_DIR")?).canonicalize()?;
    ensure!(
        markers.starts_with(&workspace),
        "probe markers must stay within repository target"
    );

    let mut assertions = negative_proofs(&endpoint).await?;
    // 故意不发 BindRequest：仅知道真实 Kik ID 不足以获得服务器下发的任务二进制。
    let mut observer = Client::data(&endpoint, &target)
        .await
        .context("unbound observer: occupy real Kik fourth data connection")?;
    std::fs::write(markers.join("ready"), b"ready")?;
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(2));
    loop {
        ensure!(
            Instant::now() < deadline,
            "parent did not complete observed task requests"
        );
        tokio::select! {
            frame = observer.reader.next() => {
                let bytes = frame.ok_or_else(|| anyhow!("unbound observer closed before verification"))??;
                match KikFrame::from_buf(bytes) {
                    Some(KikFrame::Ping | KikFrame::Pong) => {}
                    _ => bail!("unbound data connection received task data or an unexpected frame"),
                }
            }
            _ = heartbeat.tick() => observer.send(KikFrame::Pong).await?,
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                if markers.join("stop").exists() {
                    break;
                }
            }
        }
    }
    observer.close().await;
    assertions.push(
        "unbound fourth data connection receives no task bytes while real Kik completes tasks",
    );
    println!("{}", serde_json::json!({"assertions": assertions}));
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    timeout(Duration::from_secs(120), probe())
        .await
        .context("task binding probe total deadline exceeded")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, BufWriter};

    #[tokio::test]
    async fn send_flushes_a_buffered_transport_before_waiting_for_a_reply() {
        let (writer, mut wire) = tokio::io::duplex(1024);
        let reader: BoxedAsyncRead = Box::pin(tokio::io::empty());
        let mut client = Client {
            reader: FramedRead::new(
                BufReader::new(reader),
                LengthFieldBasedFrameDecoder::new_with_max_frame_len(DATA_MAX_FRAME_LENGTH),
            ),
            // 小帧全部留在缓冲区，只有显式 flush 才能在另一端读到，重现本次超时原因。
            writer: Box::pin(BufWriter::with_capacity(1024, writer)),
        };
        let expected = protocol::transfer_encode_frame(KikFrame::Ping);
        client.send(KikFrame::Ping).await.unwrap();
        let mut received = vec![0; expected.len()];
        timeout(Duration::from_secs(1), wire.read_exact(&mut received))
            .await
            .expect("the frame remained buffered instead of reaching the peer")
            .unwrap();
        assert_eq!(received.as_slice(), expected.as_ref());
    }
}
