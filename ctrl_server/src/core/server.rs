use crate::core::context::Context;
use crate::handler::read_handle;
use anyhow::Error;
use bytes::BytesMut;
use common::channel::{Channel, ChannelType};
use common::config::Config;
use common::ltc_codec::LengthFieldBasedFrameDecoder;
use common::message::frame::Frame;
use common::protocol;
use common::protocol::BufSerializable;
use log::{debug, trace};
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::{BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::Sender;
use tokio::sync::{mpsc, Mutex};
use tokio::time;
use tokio::time::timeout;
use tokio_stream::StreamExt;
use tokio_util::codec::FramedRead;
use uuid::Uuid;

pub async fn run(context: Context, config: Config) -> anyhow::Result<()> {
    let listener =
        TcpListener::bind(format!("{}:{}", config.server_host, config.server_port)).await?;
    debug!("开启服务,监听{}的{}端口",config.server_host,config.server_port);
    loop {
        let (stream, addr) = listener.accept().await?;
        tokio::spawn(handle_stream(context.clone(), config.clone(), stream, addr));
    }
}

async fn handle_stream(context: Context, config: Config, stream: TcpStream, addr: SocketAddr) {
    let (reader, writer) = stream.into_split();
    let framed_read = FramedRead::new(BufReader::new(reader), LengthFieldBasedFrameDecoder::new());
    let framed_arc = Arc::new(Mutex::new(framed_read));

    let channel_arc = Arc::new(Mutex::new(Channel::new(
        BufWriter::new(writer),
        "undefined_id".to_owned(),
        ChannelType::Unknown,
    )));

    //active逻辑
    let channel = channel_arc.clone();
    handle_active(channel.clone()).await;

    let chan = channel.clone();
    tokio::spawn(async move {
        hearbeat(chan).await;
    });

    let e = loop {
        match timeout(
            config.read_timeout,
            framed_arc.clone().lock().await.next(),
        )
        .await
        {
            Ok(res) => match res {
                Some(Ok(msg)) => {
                    let channel = channel.clone();
                    match handle_read(&config, &context, channel, msg).await {
                        Ok(_) => {}
                        Err(e) => {
                            debug!("处理消息时错误,err:{}", e);
                            break Some(e);
                        }
                    }
                    continue;
                }
                Some(Err(e)) => {
                    println!("连接异常:{}", e);
                    break Some(anyhow::Error::new(e));
                }

                None => {
                    break None;
                }
            },
            Err(e) => {
                println!("超时未读断开");
                match channel.clone().lock().await.write_half_close().await {
                    Ok(_) => {}
                    Err(_) => {}
                };
                break Some(anyhow::Error::new(e));
            }
        };
    };
    let chan = channel.clone();
    if e.is_some() {
        handle_error(chan, e.unwrap()).await;
    }

    let chan = channel.clone();
    let context = context.clone();
    tokio::spawn(async move {
        handle_inactive(context, chan).await;
    });
}

async fn handle_error(c: Arc<Mutex<Channel>>, error: Error) {
    println!("handle_error:{}", error);
}

async fn handle_inactive(context: Context, channel: Arc<Mutex<Channel>>) {
    channel.clone().lock().await.try_write_half_close().await;
    let channel_type = channel.clone().lock().await.channel_type.clone();
    match channel_type {
        ChannelType::Ctrl => {
            context.delete_ctrl_conn().await;
        }
        ChannelType::CtrlData => {
            //清理
            context.delete_ctrl_data_conn(channel).await;
        }
        ChannelType::Kik => {
            // kik连接的id直接是kikid
            let id = channel.clone().lock().await.get_id().to_string();
            context.set_kik_state();
            // 因为Kik连接断开了，所以万一在被控制，需要清理
            let _ = context.delete_kik_conn_if_id(id.as_str()).await;
        }
        ChannelType::KikData => {
            //清理
            context.delete_kik_data_conn(channel).await;
        }
        ChannelType::Unknown => {
            //清理？？？？
        }
    };
}

async fn hearbeat(channel: Arc<Mutex<Channel>>) {
    loop {
        time::sleep(Duration::from_secs(10)).await;
        // 服务器可以放开此限制
        // if channel.clone().lock().await.channel_type!=ChannelType::Unknown {
        match channel
            .clone()
            .lock()
            .await
            .write_and_flush(&protocol::ping())
            .await
        {
            Ok(_) => {}
            Err(_) => {
                break;
            }
        };
    }
}

async fn handle_active(arc: Arc<Mutex<Channel>>) {}

async fn handle_read(
    config: &Config,
    context: &Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
) -> anyhow::Result<()> {
    let channel_type = channel.clone().lock().await.channel_type.clone(); //这里不克隆直接match的话又会出现match的生命周期问题，导致死锁。
    return match channel_type {
        ChannelType::Ctrl => read_handle::handle_ctrl(context, channel, msg).await,
        ChannelType::CtrlData => read_handle::handle_ctrl_data(context, channel, msg).await,
        ChannelType::Kik => read_handle::handle_kik(context, channel, msg).await,
        ChannelType::KikData => read_handle::handle_kik_data(context, channel, msg).await,
        ChannelType::Unknown => {
            //未识别的连接连ping pong 都不让发
            read_handle::handle_init_message(config, context, channel, msg).await
        }
    };
}
