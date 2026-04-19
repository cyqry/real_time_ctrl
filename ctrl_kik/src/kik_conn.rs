use crate::context::{Context, Kik};
use crate::{cmd_runner, cmd_util, kik_data_conn, read_handle, screen};
use anyhow::Error;
use bytes::BytesMut;
use common::channel::{Channel, ChannelType};
use common::config::Config;
use common::kik_info::KikInfo;
use common::ltc_codec::LengthFieldBasedFrameDecoder;
use common::{file_util, protocol};
use common::protocol::{BufSerializable, CmdOptions};
use log::debug;
use std::any::Any;
use std::ptr::null_mut;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{channel, unbounded_channel, Receiver, Sender};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio::time::error::Elapsed;
use tokio::time::timeout;
use tokio::{join, time};
use tokio_stream::StreamExt;
use tokio_util::codec::FramedRead;
use common::command::{Command, CtrlCommand};
use common::message::init_frame::InitFrame;

pub async fn kik_conn(context: Context, config: &Config) -> anyhow::Result<JoinHandle<()>> {
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
    let name = cmd_util::whoami();
    let channel = channel_arc.clone();
    handle_active(context.clone(), name.clone(), channel.clone()).await;

    //tx在连接处理线程结束后被关闭
    let (mut tx, mut rx) = mpsc::channel::<Box<dyn Any + Send + Sync>>(5);

    let context_clone = context.clone();
    let channel_clone = channel_arc.clone();
    let handle = tokio::spawn(async move {
        let context = context_clone;
        let channel = channel_clone;
        //执行心跳逻辑
        let chan = channel.clone();
        tokio::spawn(async move {
            heartbeat(chan).await;
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
                                    debug!("读取错误");
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
                    break Some(anyhow::Error::new(e));
                }
            };
        };

        if e.is_some() {
            let chan = channel.clone();
            handle_error(chan, e.unwrap()).await;
        }
        handle_inactive(context.clone(), channel.clone()).await;
    });

    //这次为第一次rx接收数据,用于阻塞校验
    match timeout(Duration::from_secs(20), rx.recv()).await {
        Ok(recv) => {
            match recv {
                None => {
                    //连接已断开
                    //todo 报告错误
                    return Err(anyhow::Error::msg("校验时连接断开"));
                }
                Some(res) => {
                    //获得服务器分配的id
                    match res.downcast::<String>() {
                        Ok(kik_id) => {
                            {
                                //todo 这部分应该在收到消息的线程就做了，这样就算服务端迅速发完响应就迅速发ping也没有问题，但业务消息由于没有确认，还是要延迟一下(需要稍久)再发
                                let arc = channel_arc.clone();
                                let mut guard = arc.lock().await;
                                guard.channel_type = ChannelType::Kik;
                                guard.set_id(*kik_id.clone());
                            }
                            *(context.id.lock().await) = Some(*kik_id.clone());
                            context
                                .set_kik(Some(Kik::new(
                                    kik_id.to_string(),
                                    name,
                                    channel_arc.clone(),
                                )))
                                .await;
                        }
                        _ => {
                            return Err(anyhow::Error::msg("服务端奇怪的响应，系统错误"));
                        }
                    };
                }
            }
        }
        Err(e) => {
            channel.lock().await.try_write_half_close().await;
            //服务器未响应，todo 报告错误
            return Err(anyhow::Error::msg("服务器超时未响应"));
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

async fn handle_active(context: Context, name: String, channel: Arc<Mutex<Channel>>) {
    //请求之前就默认这个连接已经准备好接受对方的消息了，这种方式会导致后面收不到服务器的确认，而且在收到服务器确认前 如果收到其他除了ping pong的业务消息的话 会有这边状态(id和context)不完整的问题； 其实业务消息用到context无非就是响应，所以任意业务消息响应前收到服务器确认设置好就行，就算没设置好 顶多也就是响应超时或者让连接断开 然后kik重连；
    //如果收到确认之后再准备接收的话，那么这里服务器是无法预判你什么时候准备好了的，所以很有可能在准备接收前就发过来了非init(包括ping)消息，这边就会判断连接有问题
    //但是这里就设置为kik的话就收不到 验证请求 的 回复(KikId) 了，所以要等验证消息收完才能设置为kik
    // channel.lock().await.channel_type = ChannelType::Kik; //这里也许需要把context的维护也做了
    
    //请求被控
    channel
        .lock()
        .await
        .try_write_and_flush(&protocol::transfer_encode_frame(
            InitFrame::KikReq(KikInfo {
                id: context.id.clone().lock().await.clone(),
                name,
            }),
        ))
        .await;
    let (tx, mut rx) = unbounded_channel::<(String, CmdOptions, Command)>();
    channel.lock().await.put("cmd_tx".to_string(), tx);
    //todo 将这个线程的句柄交给连接控制主线程，方便随时杀掉；为了随时重新开新处理线程，这个线程其实应该得到命令时懒加载
    tokio::spawn(async move {
        while let Some((cmd_id,cmd_options, cmd)) = rx.recv().await {
            read_handle::handle_kik_cmd(context.clone(), &channel, cmd_id,cmd_options, cmd).await;
        }
    });

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

async fn handle_error(p0: Arc<Mutex<Channel>>, e: Error) {
    println!("handle_error:{}", e);
}

async fn handle_read(
    context: &Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
    auth_tx: &mut Sender<Box<dyn Any + Send + Sync>>,
) -> anyhow::Result<()> {
    let channel_type = channel.clone().lock().await.channel_type.clone();
    match channel_type {
        ChannelType::Kik => read_handle::handle_kik(context, channel, msg).await,
        ChannelType::Unknown => read_handle::handle_init_message(context, channel, msg, auth_tx).await,
        _ => {
            //todo 日志收集而不是 panic!
            panic!("不支持的")
        }
    }
}
