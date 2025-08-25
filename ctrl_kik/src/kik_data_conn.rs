use crate::context::Context;
use crate::{cmd_util, read_handle};
use anyhow::Error;
use bytes::BytesMut;
use common::channel::{Channel, ChannelType};
use common::config::Config;
use common::kik::Kik;
use common::kik_info::KikInfo;
use common::ltc_codec::LengthFieldBasedFrameDecoder;
use common::message::frame::Frame;
use common::message::resp::Resp;
use common::protocol;
use common::protocol::BufSerializable;
use log::debug;
use std::any::Any;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio::{join, time};
use tokio_stream::StreamExt;
use tokio_util::codec::FramedRead;
use uuid::Uuid;

pub async fn kik_data_conn(context: Context, config: &Config) -> anyhow::Result<JoinHandle<()>> {
    let socket =
        TcpStream::connect(format!("{}:{}", config.server_host, config.server_port)).await?;
    let (reader, writer) = socket.into_split();
    let framed_read = FramedRead::new(BufReader::new(reader), LengthFieldBasedFrameDecoder::new());
    let mut framed_arc = Arc::new(Mutex::new(framed_read));

    let channel_arc = Arc::new(Mutex::new(Channel::new(
        BufWriter::new(writer),
        "undefined_id".to_owned(),
        ChannelType::Unknown,
    )));

    //active逻辑
    let channel = channel_arc.clone();
    handle_active(&context, channel.clone()).await?;

    //tx在连接处理线程结束后被关闭
    let (mut tx, mut rx) = mpsc::channel::<Box<dyn Any + Send + Sync>>(5);

    let context_clone = context.clone();
    let handle = tokio::spawn(async move {
        let context = context_clone;
        //心跳逻辑
        let chan = channel.clone();
        tokio::spawn(async move {
            hearbeat(chan).await;
        });

        let e = loop {
            match timeout(
                Duration::from_secs(45),
                framed_arc.clone().lock().await.next(),
            )
            .await
            {
                //timeout返回 Ok说明读取未超时
                Ok(res) => {
                    match res {
                        Some(Ok(msg)) => {
                            //read逻辑
                            let channel = channel.clone();
                            match handle_read(&context, channel, msg, &mut tx).await {
                                Err(e) => {
                                    //说明处理读的过程中产生了错误，那么不在管这个连接
                                    break Some(e);
                                }
                                Ok(_) => {}
                            };
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
                    match channel.clone().lock().await.write_half_close().await {
                        Ok(_) => {}
                        Err(_) => {}
                    };
                    break Some(anyhow::Error::new(e));
                }
            };
        };

        if e.is_some() {
            let chan = channel.clone();
            handle_error(chan, e.unwrap()).await;
        }

        let chan = channel.clone();
        let context = context.clone();
        tokio::spawn(async move { handle_inactive(&context, chan).await });
    });

    //这次为第一次rx接收数据,用于阻塞校验
    match rx.recv().await {
        None => {
            panic!("服务器未响应")
        }
        Some(res) => {
            //获得服务器响应的kik_id
            match res.downcast::<String>() {
                Ok(kik_id) => {
                    {
                        let arc = channel_arc.clone();
                        let mut guard = arc.lock().await;
                        guard.put("kik_id".to_string(), *kik_id.clone());
                        guard.set_id(Uuid::new_v4().to_string());
                        guard.channel_type = ChannelType::KikData;
                    }
                    context.insert_data_conn(channel_arc.clone()).await;
                }
                _ => {
                    panic!("服务端奇怪的响应，系统错误");
                }
            };
        }
    };
    Ok(handle)
}

async fn hearbeat(channel: Arc<Mutex<Channel>>) {
    loop {
        time::sleep(Duration::from_secs(5)).await;
        let arc = channel.clone();
        let mut guard = arc.lock().await;
        if guard.channel_type != ChannelType::Unknown {
            match guard.write_and_flush(&protocol::pong()).await {
                Ok(_) => {}
                Err(_) => {
                    break;
                }
            };
        }
    }
}

async fn handle_active(context: &Context, channel: Arc<Mutex<Channel>>) -> anyhow::Result<()> {
    let id = context.id.clone().lock().await.as_ref().unwrap().clone();
    channel
        .clone()
        .lock()
        .await
        .write_and_flush(&protocol::transfer_encode(
            Frame::KikDataConnReq(id).to_buf(),
        ))
        .await
}

async fn handle_inactive(context: &Context, c: Arc<Mutex<Channel>>) {
    c.clone().lock().await.try_write_half_close().await;
    context.delete_data_conn(c).await;
}

async fn handle_error(p0: Arc<Mutex<Channel>>, e: Error) {
    println!("handle_error:{}", e);
}

async fn handle_read(
    context: &Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
    tx: &mut Sender<Box<dyn Any + Send + Sync>>,
) -> anyhow::Result<()> {
    let channel_type = channel.clone().lock().await.channel_type.clone();
    match channel_type {
        ChannelType::KikData => read_handle::handle_kik_data(context, channel, msg, tx).await,
        ChannelType::Unknown => read_handle::handle_init_message(context, channel, msg, tx).await,
        _ => {
            //todo 日志收集而不是 panic!
            panic!("不支持的")
        }
    }
}
