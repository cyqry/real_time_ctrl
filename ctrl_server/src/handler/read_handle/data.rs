use crate::core::context::Context;
use bytes::BytesMut;
use common::channel::Channel;
use common::message::kik_frame::{encode_data_frame as encode_kik_data_frame, KikFrame};
use common::protocol::BufSerializable;
use ctrl_common::ctrl_frame::{encode_data_frame as encode_ctrl_data_frame, Frame};
use log::info;
use std::sync::Arc;
use tokio::sync::Mutex;

fn default_error() -> anyhow::Error {
    anyhow::Error::msg("不支持的数据帧类型")
}
pub async fn handle_ctrl_data(
    context: Context,
    _: Arc<Mutex<Channel>>,
    msg: BytesMut,
) -> anyhow::Result<()> {
    match Frame::from_buf(msg).ok_or_else(default_error)? {
        Frame::Data(data_id, data) => {
            let encoded = encode_kik_data_frame(&data_id, &data)?;
            if let Some(kik) = context.get_kik().await {
                let connections = kik.data_connections_for_send().await;
                if connections.is_empty() {
                    info!("当前kik没有数据连接")
                } else {
                    write_to_available_connection(connections, &encoded).await?;
                }
            } else {
                info!("数据发送失败当前没有在线kik")
            }
        }
        Frame::Ping | Frame::Pong => {}
        _ => return Err(default_error()),
    }
    Ok(())
}

pub async fn handle_kik_data(
    context: Context,
    _channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
) -> anyhow::Result<()> {
    match KikFrame::from_buf(msg).ok_or_else(default_error)? {
        KikFrame::Data(data_id, data) => {
            let connections = context.ctrl_data_connections_for_send().await;
            if !connections.is_empty() {
                let encoded = encode_ctrl_data_frame(&data_id, &data)?;
                write_to_available_connection(connections, &encoded).await?;
            }
        }
        KikFrame::Ping | KikFrame::Pong => {}
        _ => return Err(default_error()),
    }
    Ok(())
}

/// 单个完整帧在首选连接失败后尝试其余连接。
///
/// 写失败可能发生在对端已收到完整帧之后，因此文件接收端必须把完全相同的
/// 区间视为幂等重试；部分重叠仍按协议错误拒绝。
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
