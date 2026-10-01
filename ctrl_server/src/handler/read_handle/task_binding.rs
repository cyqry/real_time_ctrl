//! 任务数据连接的挑战绑定。所有证明只在本条连接、当前主连接密钥和短期截止时间内有效。
//! 旧 Kik 数据连接继续支持原文件功能；只有通过此证明的连接才能接收任务程序。

use crate::core::{connection_meta::KIK_ID, context::Context};
use common::{
    channel::{Channel, ChannelAttributeKey},
    message::kik_frame::KikFrame,
    protocol::transfer_encode_frame,
    task::{self, TaskFrame, TASK_DATA_BINDING, TASK_MAIN_KEY},
};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;

/// Option 在成功或失败验证时消耗，防止同一挑战被二次使用。
type BindingChallenge = Option<([u8; 32], [u8; 32], Instant)>;
const CHALLENGE: ChannelAttributeKey<BindingChallenge> =
    ChannelAttributeKey::new(0x7461_736b_6368_616c);

pub async fn handle(
    context: &Context,
    channel: &Arc<Mutex<Channel>>,
    frame: TaskFrame,
) -> anyhow::Result<()> {
    let kik_id = channel
        .lock()
        .await
        .attribute(&KIK_ID)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("任务数据连接缺少 Kik 绑定"))?;
    let kik = context
        .get_initialized_kik_by_id(&kik_id)
        .await
        .ok_or_else(|| anyhow::anyhow!("任务数据连接没有活动主连接"))?;
    let main = kik
        .get_kik_conn()
        .await
        .ok_or_else(|| anyhow::anyhow!("Kik 主连接已关闭"))?;
    let key = main
        .lock()
        .await
        .attribute(&TASK_MAIN_KEY)
        .copied()
        .ok_or_else(|| anyhow::anyhow!("Kik 未声明任务能力"))?;
    match frame {
        TaskFrame::BindRequest => {
            let mut connection = channel.lock().await;
            if connection.attribute(&CHALLENGE).is_some()
                || connection.attribute(&TASK_DATA_BINDING).is_some()
            {
                anyhow::bail!("任务数据连接不能重复绑定");
            }
            let nonce = task::random_key();
            connection.insert_attribute(
                &CHALLENGE,
                Some((nonce, task::binding_id(&key), Instant::now())),
            );
            connection
                .write_and_flush(&transfer_encode_frame(KikFrame::Task(
                    TaskFrame::Challenge(nonce),
                )))
                .await?;
        }
        TaskFrame::Proof(proof) => {
            let mut connection = channel.lock().await;
            let challenge = connection.attribute(&CHALLENGE).cloned().flatten();
            connection.insert_attribute(&CHALLENGE, None);
            let (nonce, generation, issued) =
                challenge.ok_or_else(|| anyhow::anyhow!("缺少任务数据挑战"))?;
            if issued.elapsed() > Duration::from_secs(30)
                || generation != task::binding_id(&key)
                || !task::verify_binding(&key, &nonce, &proof)
            {
                anyhow::bail!("任务数据归属证明错误或已过期");
            }
            connection.insert_attribute(&TASK_DATA_BINDING, generation);
            connection
                .write_and_flush(&transfer_encode_frame(KikFrame::Task(TaskFrame::Bound)))
                .await?;
        }
        _ => anyhow::bail!("此任务帧不属于数据通道"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::channel::ChannelType;
    use ctrl_common::kik::Kik;

    fn connection(id: &str, kind: ChannelType) -> (Arc<Mutex<Channel>>, tokio::io::DuplexStream) {
        let (peer, stream) = tokio::io::duplex(4096);
        let channel = Channel::new(
            Box::pin(stream),
            Some(id.into()),
            kind,
            Err(std::io::Error::other("test")),
            Err(std::io::Error::other("test")),
        );
        (Arc::new(Mutex::new(channel)), peer)
    }

    #[tokio::test]
    async fn proofs_are_single_use_connection_bound_and_reconnect_invalidates_challenge() {
        let context = Context::init();
        let (main, _main_peer) = connection("kik", ChannelType::Kik);
        let key = task::random_key();
        main.lock().await.insert_attribute(&TASK_MAIN_KEY, key);
        let kik = Kik::new(
            "kik",
            "test",
            "127.0.0.1".into(),
            std::time::SystemTime::now(),
            main.clone(),
        );
        kik.set_kik_initialized(true);
        context.kiks.write().await.insert("kik".into(), kik);
        let (data, _data_peer) = connection("data", ChannelType::KikData);
        data.lock().await.insert_attribute(&KIK_ID, "kik".into());
        handle(&context, &data, TaskFrame::BindRequest)
            .await
            .unwrap();
        let nonce = data.lock().await.attribute(&CHALLENGE).unwrap().unwrap().0;
        let proof = task::binding_proof(&key, &nonce);
        let (other, _other_peer) = connection("other", ChannelType::KikData);
        other.lock().await.insert_attribute(&KIK_ID, "kik".into());
        handle(&context, &other, TaskFrame::BindRequest)
            .await
            .unwrap();
        assert!(handle(&context, &other, TaskFrame::Proof(proof))
            .await
            .is_err());
        assert!(other.lock().await.attribute(&TASK_DATA_BINDING).is_none());
        handle(&context, &data, TaskFrame::Proof(proof))
            .await
            .unwrap();
        assert_eq!(
            data.lock().await.attribute(&TASK_DATA_BINDING),
            Some(&task::binding_id(&key))
        );
        assert!(handle(&context, &data, TaskFrame::Proof(proof))
            .await
            .is_err());

        let (stale, _stale_peer) = connection("stale", ChannelType::KikData);
        stale.lock().await.insert_attribute(&KIK_ID, "kik".into());
        handle(&context, &stale, TaskFrame::BindRequest)
            .await
            .unwrap();
        let nonce = stale.lock().await.attribute(&CHALLENGE).unwrap().unwrap().0;
        main.lock()
            .await
            .insert_attribute(&TASK_MAIN_KEY, task::random_key());
        assert!(handle(
            &context,
            &stale,
            TaskFrame::Proof(task::binding_proof(&key, &nonce))
        )
        .await
        .is_err());
        assert!(stale.lock().await.attribute(&TASK_DATA_BINDING).is_none());

        let current_key = *main.lock().await.attribute(&TASK_MAIN_KEY).unwrap();
        let (expired, _expired_peer) = connection("expired", ChannelType::KikData);
        expired.lock().await.insert_attribute(&KIK_ID, "kik".into());
        handle(&context, &expired, TaskFrame::BindRequest)
            .await
            .unwrap();
        let nonce = expired
            .lock()
            .await
            .attribute(&CHALLENGE)
            .unwrap()
            .unwrap()
            .0;
        expired.lock().await.insert_attribute(
            &CHALLENGE,
            Some((
                nonce,
                task::binding_id(&current_key),
                Instant::now() - Duration::from_secs(31),
            )),
        );
        assert!(handle(
            &context,
            &expired,
            TaskFrame::Proof(task::binding_proof(&current_key, &nonce))
        )
        .await
        .is_err());
        assert!(expired.lock().await.attribute(&TASK_DATA_BINDING).is_none());
    }
}
