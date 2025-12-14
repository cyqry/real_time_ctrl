use crate::context::Context;
use bytes::BytesMut;
use common::channel::{Channel, ChannelType};
use common::config::{Config, Id};
use common::ltc_codec::LengthFieldBasedFrameDecoder;
use common::message::frame::Frame;
use common::message::resp::Resp;
use common::protocol;
use common::protocol::BufSerializable;
use log::debug;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio::time;
use tokio::time::timeout;
use tokio_stream::StreamExt;
use tokio_util::codec::FramedRead;

pub async fn ctrl_conn(config: &Config) -> anyhow::Result<(Arc<Mutex<Channel>>, Receiver<Resp>)> {
    let socket =
        TcpStream::connect(format!("{}:{}", config.server_host, config.server_port)).await?;
    let (reader, writer) = socket.into_split();
    let framed_read = FramedRead::new(BufReader::new(reader), LengthFieldBasedFrameDecoder::new());
    let mut framed_arc = Arc::new(Mutex::new(framed_read));

    let channel_arc = Arc::new(Mutex::new(Channel::new(
        writer,
        None,
        ChannelType::Unknown,
    )));

    //active逻辑
    let channel = channel_arc.clone();
    handle_active(&config.id, channel.clone()).await?;

    //tx在连接处理线程结束后被关闭
    let (mut tx, mut rx) = mpsc::channel::<Resp>(5);
    tokio::spawn(async move {
        //心跳逻辑
        let chan = channel.clone();
        tokio::spawn(async move {
            hearbeat(chan).await;
        });

        loop {
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
                            match handle_read(channel, msg, &mut tx).await {
                                None => {
                                    debug!("读取错误");
                                    //说明处理读的过程中产生了错误，那么不在管这个连接
                                    break;
                                }
                                Some(_) => {}
                            };
                            continue;
                        }
                        Some(Err(e)) => {
                            println!("连接异常:{}", e);
                            //异常时处理
                            let channel = channel.clone();
                            handle_error(channel).await;
                            break;
                        }
                        //对方正常关闭
                        None => {
                            //不在这里对正常关闭进行特殊处理
                            break;
                        }
                    }
                }
                Err(_) => {
                    println!("超时未读断开");
                    match channel.clone().lock().await.write_half_close().await {
                        Ok(_) => {}
                        Err(_) => {}
                    };
                    break;
                }
            };
        }
        let chan = channel.clone();
        tokio::spawn(async move { handle_inactive(chan).await });
    });

    //这次为第一次rx接收数据,用于阻塞校验
    match rx.recv().await {
        None => {
            panic!("服务器未响应")
        }
        Some(res) => {
            match res {
                Resp::Info(auth) => {
                    //使用这个类型这个值来标识这是服务端校验成功的回复
                    if auth == "##authtrue" {
                        //更改类型为Ctrl，即校验成功了
                        channel_arc.clone().lock().await.channel_type = ChannelType::Ctrl;
                        println!("校验成功");
                    } else {
                        panic!("服务端奇怪的响应，系统错误");
                    }
                }
                _ => {
                    panic!("服务端奇怪的响应，系统错误");
                }
            };
        }
    };

    Ok((channel_arc, rx))
}

async fn data_conn() {}

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

async fn handle_active(id: &Id, channel: Arc<Mutex<Channel>>) -> anyhow::Result<()> {
    channel
        .clone()
        .lock()
        .await
        .write_and_flush(&protocol::transfer_encode_frame(
            Frame::CtrlAuthReq(id.encrypt()),
        ))
        .await?;
    Ok(())
}

async fn handle_error(channel: Arc<Mutex<Channel>>) {}

async fn handle_inactive(channel: Arc<Mutex<Channel>>) {
    match channel.clone().lock().await.write_half_close().await {
        Ok(_) => {}
        Err(_) => {}
    }
}

async fn handle_read(
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
    tx: &mut Sender<Resp>,
) -> Option<()> {
    let frame = Frame::from_buf(msg)?;

    match frame {
        Frame::CtrlAuthReply(b) => {
            if !b {
                debug!("控制连接校验失败");
                println!("账号或密码错误");
                std::process::exit(0);
            } else {
                //发一次使得这个的rx第一次read，即接下来可以read数据
                tx.send(Resp::Info("##authtrue".to_string())).await.unwrap(); //设计不允许此时unwrap
            }
        }
        Frame::Resp(resp) => {
            tx.send(resp).await.ok()?;
        }
        Frame::Ping => {}
        Frame::Pong => {}
        f => {
            debug!("控制连接收到错误的帧,{:?}", f);
            panic!("控制端不支持的帧")
        }
    };
    Some(())
}
