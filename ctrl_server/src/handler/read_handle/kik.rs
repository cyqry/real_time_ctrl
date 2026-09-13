use crate::core::context::Context;
use bytes::BytesMut;
use common::channel::Channel;
use common::message::kik_frame::KikFrame;
use common::protocol::BufSerializable;
use log::{debug, warn};
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
                warn!("丢弃已被替换的旧 Kik 连接响应: kik_id={}", kik_id);
                return Ok(());
            }
            debug!("收到 Kik 响应: kik_id={}, cmd_id={}", kik_id, command_id);
            if !kik.complete_command(&command_id, response).await {
                // 超时或伪造关联 ID 不会影响其他请求，只记录并丢弃。
                warn!(
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
