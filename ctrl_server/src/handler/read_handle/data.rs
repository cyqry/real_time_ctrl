//! CtrlData 与 KikData 之间的数据帧转发。
//!
//! 每帧先查 `Context` 中绑定会话、Kik 和方向的路由，再把外部 ID 与内部 ID 相互改写。转发会等待
//! 下游写入形成背压；一条连接失败时只在当前有界连接快照内重试，不创建无界任务。

use crate::core::connection_meta::{CTRL_SESSION_ID, KIK_ID};
use crate::core::context::Context;
use bytes::BytesMut;
use common::channel::Channel;
use common::message::kik_frame::{encode_data_frame as encode_kik_data_frame, KikFrame};
use common::protocol::BufSerializable;
use ctrl_common::ctrl_frame::{encode_data_frame as encode_ctrl_data_frame, Frame};
use log::debug;
use std::sync::Arc;
use tokio::sync::Mutex;

fn default_error() -> anyhow::Error {
    anyhow::Error::msg("不支持的数据帧类型")
}

pub async fn handle_ctrl_data(
    context: Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
) -> anyhow::Result<()> {
    match Frame::from_buf(msg).ok_or_else(default_error)? {
        Frame::Data(external_id, data) => {
            let session_id = channel
                .lock()
                .await
                .attribute(&CTRL_SESSION_ID)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("控制数据通道缺少会话绑定"))?;
            let Some((kik, wire_id, single_frame)) = context
                .wait_ctrl_data_target(&session_id, &external_id)
                .await
            else {
                // 未注册、过期或属于其他会话的数据一律拒绝，防止跨租户注入。
                anyhow::bail!("数据关联 ID 未授权或已过期");
            };
            let encoded = encode_kik_data_frame(&wire_id, &data)?;
            write_to_available_connection(kik.data_connections_for_send().await, &encoded).await?;
            if single_frame {
                context
                    .complete_ctrl_single_frame_route(&session_id, &external_id)
                    .await;
            }
        }
        Frame::Ping | Frame::Pong => {}
        _ => return Err(default_error()),
    }
    Ok(())
}

pub async fn handle_kik_data(
    context: Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
) -> anyhow::Result<()> {
    match KikFrame::from_buf(msg).ok_or_else(default_error)? {
        KikFrame::Data(wire_id, data) => {
            let kik_id = channel
                .lock()
                .await
                .attribute(&KIK_ID)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("Kik 数据通道缺少 Kik ID"))?;
            let Some((session_id, external_id, single_frame)) =
                context.kik_data_target(&kik_id, &wire_id).await
            else {
                debug!(
                    "丢弃未注册或跨 Kik 的数据帧: kik_id={}, data_id={}",
                    kik_id, wire_id
                );
                return Ok(());
            };
            let connections = context.ctrl_data_connections_for_send(&session_id).await;
            if connections.is_empty() {
                anyhow::bail!("目标控制会话没有可用数据通道");
            }
            let encoded = encode_ctrl_data_frame(&external_id, &data)?;
            write_to_available_connection(connections, &encoded).await?;
            if single_frame {
                context.complete_kik_single_frame_route(&wire_id).await;
            }
        }
        KikFrame::Ping | KikFrame::Pong => {}
        _ => return Err(default_error()),
    }
    Ok(())
}

/// 单帧写失败后尝试其余连接。接收端以区间为幂等键，所以完整帧重试是安全的。
async fn write_to_available_connection(
    connections: Vec<Arc<Mutex<Channel>>>,
    encoded: &[u8],
) -> anyhow::Result<()> {
    let mut last_error = None;
    for connection in connections {
        let mut connection = connection.lock().await;
        if connection.is_closed() {
            continue;
        }
        match connection.write_and_flush(encoded).await {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("没有可用的数据转发连接")))
}
