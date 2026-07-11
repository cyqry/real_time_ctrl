use crate::context::Context;
use crate::read_handle;
use anyhow::{anyhow, Error};
use bytes::BytesMut;
use common::channel::{Channel, ChannelType};
use common::config::Config;
use common::ltc_codec::{
    LengthFieldBasedFrameDecoder, DATA_MAX_FRAME_LENGTH, INIT_MAX_FRAME_LENGTH,
};
use common::message::init_frame::InitFrame;
use common::protocol;
use log::debug;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::BufReader;
use tokio::net::TcpStream;
use tokio::sync::mpsc::Sender;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio::time;
use tokio::time::timeout;
use tokio_stream::StreamExt;
use tokio_util::codec::FramedRead;
use uuid::Uuid;

pub async fn kik_data_conn(context: Context, config: &Config) -> anyhow::Result<JoinHandle<()>> {
    let endpoint = format!("{}:{}", config.server_host, config.server_port);
    let socket = timeout(config.read_timeout, TcpStream::connect(&endpoint))
        .await
        .map_err(|_| anyhow!("连接服务端数据端口超时"))??;
    socket.set_nodelay(true)?;
    let (reader, writer) = socket.into_split();
    // Kik 数据通道承载截图和文件内容，先保留兼容旧路径的大帧上限。
    let framed_read = FramedRead::new(
        BufReader::new(reader),
        LengthFieldBasedFrameDecoder::new_with_max_frame_len(INIT_MAX_FRAME_LENGTH),
    );
    let framed_arc = Arc::new(Mutex::new(framed_read));

    let channel_arc = Arc::new(Mutex::new(Channel::from_tcp_writer(
        writer,
        None,
        ChannelType::Unknown,
    )));
    channel_arc
        .lock()
        .await
        .set_write_timeout(config.write_timeout);

    //active逻辑
    let channel = channel_arc.clone();
    handle_active(&context, channel.clone()).await?;

    //tx在连接处理线程结束后被关闭
    let (mut tx, mut rx) = mpsc::channel::<String>(1);

    let context_clone = context.clone();
    let read_timeout = config.read_timeout;
    let handle = tokio::spawn(async move {
        let context = context_clone;
        //心跳逻辑
        let chan = channel.clone();
        tokio::spawn(async move {
            hearbeat(chan).await;
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
                                break Some(error);
                            }
                            if channel.lock().await.channel_type == ChannelType::KikData {
                                framed_arc
                                    .lock()
                                    .await
                                    .decoder_mut()
                                    .set_max_frame_len(DATA_MAX_FRAME_LENGTH);
                            }
                            continue;
                        }
                        Some(Err(e)) => {
                            println!("连接异常:{}", e);
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
                    println!("超时未读断开");
                    let _ = channel.clone().lock().await.write_half_close().await;
                    break Some(anyhow::Error::new(e));
                }
            };
        };

        if let Some(error) = e {
            let chan = channel.clone();
            handle_error(chan, error).await;
        }

        handle_inactive(&context, channel).await;
    });

    //这次为第一次rx接收数据,用于阻塞校验
    match timeout(config.read_timeout, rx.recv())
        .await
        .map_err(|_| anyhow!("等待数据连接初始化响应超时"))?
    {
        None => {
            return Err(anyhow!("发送端关闭，连接结束"));
        }
        Some(_kik_id) => {
            {
                let mut guard = channel_arc.lock().await;
                guard.set_id(Uuid::new_v4().to_string());
                guard.channel_type = ChannelType::KikData;
            }
            context.insert_data_conn(channel_arc.clone()).await?;
        }
    };
    Ok(handle)
}

async fn hearbeat(channel: Arc<Mutex<Channel>>) {
    loop {
        time::sleep(Duration::from_secs(5)).await;
        let arc = channel.clone();
        if arc.lock().await.is_closed() {
            return;
        }

        let mut guard = arc.lock().await;
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

async fn handle_active(context: &Context, channel: Arc<Mutex<Channel>>) -> anyhow::Result<()> {
    let id = context
        .id
        .lock()
        .await
        .clone()
        .ok_or_else(|| anyhow!("命令连接尚未取得 kik id"))?;
    channel
        .clone()
        .lock()
        .await
        .write_and_flush(&protocol::transfer_encode_frame(InitFrame::KikDataConnReq(
            id,
        )))
        .await
}

async fn handle_inactive(context: &Context, c: Arc<Mutex<Channel>>) {
    c.clone().lock().await.try_write_half_close().await;
    context.delete_data_conn(c).await;
}

async fn handle_error(_channel: Arc<Mutex<Channel>>, e: Error) {
    debug!("handle_error:{}", e);
}

async fn handle_read(
    context: &Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
    auth_tx: &mut Sender<String>,
) -> anyhow::Result<()> {
    let channel_type = channel.lock().await.channel_type;
    match channel_type {
        ChannelType::KikData => read_handle::handle_kik_data(context, channel, msg).await,
        ChannelType::Unknown => {
            read_handle::handle_init_message(context, channel.clone(), msg, auth_tx).await?;
            channel.lock().await.channel_type = ChannelType::KikData;
            Ok(())
        }
        _ => Err(anyhow!("数据连接状态与帧类型不匹配")),
    }
}
