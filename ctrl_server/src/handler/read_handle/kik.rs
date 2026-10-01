//! 已认证 Kik 主连接的响应路由。
//!
//! 服务端只接受当前主连接返回的响应，并按内部命令 ID 唤醒唯一等待者。旧重连、未知 ID 或已超时响应
//! 会被丢弃，不能误投递给其他控制会话。

use crate::core::context::Context;
use bytes::BytesMut;
use common::channel::Channel;
use common::message::kik_frame::KikFrame;
use common::protocol::BufSerializable;
use log::debug;
use std::sync::Arc;
use tokio::sync::Mutex;

fn default_error() -> anyhow::Error {
    anyhow::Error::msg("不支持的 Kik 业务帧类型")
}

pub async fn handle_kik(
    context: Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
) -> anyhow::Result<()> {
    match KikFrame::from_buf(msg).ok_or_else(|| anyhow::anyhow!("帧格式错误"))? {
        KikFrame::Task(frame) => {
            use common::task::{
                TaskFrame, TASK_CACHE_CAPABLE, TASK_MAIN_KEY, TASK_NAMED_CACHE_CAPABLE,
            };
            let kik_id = channel
                .lock()
                .await
                .id()
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("Kik 缺少 ID"))?;
            let kik = context
                .get_initialized_kik_by_id(&kik_id)
                .await
                .ok_or_else(|| anyhow::anyhow!("Kik 未就绪"))?;
            if !kik.is_kik_conn(&channel).await {
                return Ok(());
            }
            let cache_capable = matches!(&frame, TaskFrame::HelloCached(_));
            let named_capable = matches!(&frame, TaskFrame::HelloNamed(_));
            match frame {
                TaskFrame::Hello(key)
                | TaskFrame::HelloCached(key)
                | TaskFrame::HelloNamed(key) => {
                    let mut connection = channel.lock().await;
                    if connection.attribute(&TASK_MAIN_KEY).is_some() {
                        anyhow::bail!("重复任务能力声明");
                    }
                    connection.insert_attribute(&TASK_MAIN_KEY, key);
                    connection.insert_attribute(&TASK_CACHE_CAPABLE, cache_capable);
                    connection.insert_attribute(&TASK_NAMED_CACHE_CAPABLE, named_capable);
                    connection
                        .write_and_flush(&common::protocol::transfer_encode_frame(KikFrame::Task(
                            if named_capable {
                                TaskFrame::HelloNamedAck
                            } else if cache_capable {
                                TaskFrame::HelloCachedAck
                            } else {
                                TaskFrame::HelloAck
                            },
                        )))
                        .await?;
                }
                TaskFrame::Ready(id) => {
                    kik.complete_command(
                        &format!("{id}-ready"),
                        super::task_run::preparation_receipt(false),
                    )
                    .await;
                }
                TaskFrame::CacheHit(id) => {
                    if channel.lock().await.attribute(&TASK_NAMED_CACHE_CAPABLE) != Some(&true) {
                        anyhow::bail!("未协商指定文件名任务缓存能力，请升级 Kik");
                    }
                    kik.complete_command(
                        &format!("{id}-ready"),
                        super::task_run::preparation_receipt(true),
                    )
                    .await;
                }
                _ => return Err(default_error()),
            }
        }
        KikFrame::RespExtra(response, command_id) => {
            let kik_id = channel
                .lock()
                .await
                .id()
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("Kik 响应连接缺少 ID"))?;
            let kik = context
                .get_initialized_kik_by_id(&kik_id)
                .await
                .ok_or_else(|| anyhow::anyhow!("Kik 响应来自非活动连接"))?;
            if !kik.is_kik_conn(&channel).await {
                debug!("丢弃已被替换的旧 Kik 连接响应: kik_id={}", kik_id);
                return Ok(());
            }
            debug!("收到 Kik 响应: kik_id={}, cmd_id={}", kik_id, command_id);
            if !kik.complete_command(&command_id, response).await {
                // 超时或伪造关联 ID 不会影响其他请求，只记录并丢弃。
                debug!(
                    "丢弃无等待者的 Kik 响应: kik_id={}, cmd_id={}",
                    kik_id, command_id
                );
            }
        }
        KikFrame::Ping | KikFrame::Pong => {}
        _ => return Err(default_error()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::{
        channel::ChannelType,
        message::kik_resp::{ClientSuccessResp, KikResp},
        task::{TaskFrame, TASK_CACHE_CAPABLE, TASK_MAIN_KEY, TASK_NAMED_CACHE_CAPABLE},
    };
    use ctrl_common::kik::Kik;
    use tokio::io::{AsyncReadExt, DuplexStream};

    async fn connected() -> (Context, Kik, Arc<Mutex<Channel>>, DuplexStream) {
        let context = Context::init();
        let (peer, stream) = tokio::io::duplex(4096);
        let channel = Arc::new(Mutex::new(Channel::new(
            Box::pin(stream),
            Some("kik".into()),
            ChannelType::Kik,
            Err(std::io::Error::other("test")),
            Err(std::io::Error::other("test")),
        )));
        let kik = Kik::new(
            "kik",
            "test",
            "127.0.0.1".into(),
            std::time::SystemTime::now(),
            channel.clone(),
        );
        kik.set_kik_initialized(true);
        context.kiks.write().await.insert("kik".into(), kik.clone());
        (context, kik, channel, peer)
    }

    #[tokio::test]
    async fn all_hello_generations_have_distinct_acknowledgements_and_cannot_upgrade_in_place() {
        for generation in 0..3 {
            let cached = generation == 1;
            let named = generation == 2;
            let (context, _kik, channel, mut peer) = connected().await;
            let hello = if named {
                TaskFrame::HelloNamed([8; 32])
            } else if cached {
                TaskFrame::HelloCached([8; 32])
            } else {
                TaskFrame::Hello([8; 32])
            };
            handle_kik(
                context.clone(),
                channel.clone(),
                KikFrame::Task(hello.clone()).to_buf(),
            )
            .await
            .unwrap();
            assert_eq!(
                channel.lock().await.attribute(&TASK_MAIN_KEY),
                Some(&[8; 32])
            );
            assert_eq!(
                channel.lock().await.attribute(&TASK_CACHE_CAPABLE),
                Some(&cached)
            );
            assert_eq!(
                channel.lock().await.attribute(&TASK_NAMED_CACHE_CAPABLE),
                Some(&named)
            );
            let length = peer.read_u32().await.unwrap();
            let mut response = vec![0; length as usize];
            peer.read_exact(&mut response).await.unwrap();
            let response = KikFrame::from_buf(response.as_slice().into()).unwrap();
            assert!(matches!(response, KikFrame::Task(TaskFrame::HelloCachedAck)) == cached);
            assert!(matches!(response, KikFrame::Task(TaskFrame::HelloAck)) == (generation == 0));
            assert!(matches!(response, KikFrame::Task(TaskFrame::HelloNamedAck)) == named);
            for repeated in [
                TaskFrame::Hello([9; 32]),
                TaskFrame::HelloCached([9; 32]),
                TaskFrame::HelloNamed([9; 32]),
            ] {
                assert!(handle_kik(
                    context.clone(),
                    channel.clone(),
                    KikFrame::Task(repeated).to_buf()
                )
                .await
                .is_err());
            }
            assert_eq!(
                channel.lock().await.attribute(&TASK_MAIN_KEY),
                Some(&[8; 32])
            );
            assert_eq!(
                channel.lock().await.attribute(&TASK_NAMED_CACHE_CAPABLE),
                Some(&named)
            );
        }
    }

    #[tokio::test]
    async fn cache_hit_requires_negotiation_and_only_completes_the_preparation_waiter() {
        for named in [false, true] {
            let (context, kik, channel, _peer) = connected().await;
            channel
                .lock()
                .await
                .insert_attribute(&TASK_NAMED_CACHE_CAPABLE, named);
            // 即使声明了旧缓存协议，也不能把旧 CacheHit 当作新命名协议回执。
            channel
                .lock()
                .await
                .insert_attribute(&TASK_CACHE_CAPABLE, true);
            let mut ready = kik.register_command("run-ready".into()).await.unwrap();
            let mut result = kik.register_command("run".into()).await.unwrap();
            let handled = handle_kik(
                context.clone(),
                channel.clone(),
                KikFrame::Task(TaskFrame::CacheHit("run".into())).to_buf(),
            )
            .await;
            assert_eq!(handled.is_ok(), named);
            if named {
                assert!(
                    matches!(ready.await.unwrap(), KikResp::Success(ClientSuccessResp::Info(value)) if value == "cache-hit")
                );
            } else {
                assert!(matches!(
                    ready.try_recv(),
                    Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                ));
                handle_kik(
                    context,
                    channel,
                    KikFrame::Task(TaskFrame::Ready("run".into())).to_buf(),
                )
                .await
                .unwrap();
                assert!(
                    matches!(ready.await.unwrap(), KikResp::Success(ClientSuccessResp::Info(value)) if value.is_empty())
                );
            }
            assert!(matches!(
                result.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            ));
            kik.cancel_command("run").await;
        }
    }

    #[tokio::test]
    async fn replaced_main_cannot_negotiate_or_complete_new_generation_task_waiters() {
        let (context, old_kik, old_channel, _old_peer) = connected().await;
        let (_unused, new_kik, new_channel, _new_peer) = connected().await;
        context
            .kiks
            .write()
            .await
            .insert("kik".into(), new_kik.clone());
        handle_kik(
            context.clone(),
            old_channel.clone(),
            KikFrame::Task(TaskFrame::HelloNamed([9; 32])).to_buf(),
        )
        .await
        .unwrap();
        assert!(old_channel
            .lock()
            .await
            .attribute(&TASK_NAMED_CACHE_CAPABLE)
            .is_none());
        assert!(new_channel
            .lock()
            .await
            .attribute(&TASK_NAMED_CACHE_CAPABLE)
            .is_none());
        let mut waiter = new_kik.register_command("run-ready".into()).await.unwrap();
        old_channel
            .lock()
            .await
            .insert_attribute(&TASK_NAMED_CACHE_CAPABLE, true);
        handle_kik(
            context,
            old_channel,
            KikFrame::Task(TaskFrame::CacheHit("run".into())).to_buf(),
        )
        .await
        .unwrap();
        assert!(matches!(
            waiter.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        new_kik.cancel_command("run-ready").await;
        assert!(old_kik.id().is_some());
    }
}
