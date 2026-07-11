use crate::core::connection_meta::KIK_RESPONSE_TX;
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
    let frame = KikFrame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    match frame {
        KikFrame::RespExtra(resp, cmd_id) => {
            debug!("handle kik,kik响应:{:?},cmd_id:{}", resp, cmd_id);
            let tx = channel
                .lock()
                .await
                .attribute(&KIK_RESPONSE_TX)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("被控端响应发送队列未初始化"))?;
            match context.active_command_id().await {
                None => {
                    //过期id或异常id,不处理
                    return Ok(());
                }
                Some(id) => {
                    if cmd_id != id {
                        //过期id或异常id,不处理
                        return Ok(());
                    }
                }
            };
            //这里可能发生cmd_id改变
            tx.send((resp, cmd_id))
                .await
                .map_err(|_| anyhow::anyhow!("被控端响应接收任务已关闭"))?;
        }
        KikFrame::Ping => {}
        KikFrame::Pong => {}
        _ => {
            return Err(default_error());
        }
    }
    Ok(())
}

// channel的id在 此方法中初始化
// return err会跳出循环关闭连接
