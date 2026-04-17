use crate::core::context::Context;
use bytes::BytesMut;
use common::channel::{Channel, ChannelType};
use common::command::{Command, CtrlCommand, SysCommand};
use common::config::Config;
use common::kik::Kik;
use common::kik_info::KikInfo;
use common::message::init_frame::InitFrame;
use common::message::kik_frame::KikFrame;
use common::message::kik_resp;
use common::protocol::{BufSerializable, ReqCmd};
use common::{async_util, protocol};
use ctrl_common::ctrl_frame::Frame;
use ctrl_common::ctrl_protocol::{
    ctrl_kik_resp, ctrl_server_resp, ctrl_server_resp_error, ctrl_server_resp_success,
};
use ctrl_common::ctrl_resp::ServerResp;
use log::{debug, info, warn};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{mpsc, Mutex};
use tokio::time::timeout;
use tokio_fusion::{Task, ThreadPool, ThreadPoolConfig};
use uuid::Uuid;

fn default_error() -> anyhow::Error {
    anyhow::Error::msg("不支持的帧类型")
}

pub async fn handle_ctrl(
    context: Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
) -> anyhow::Result<()> {
    //todo 这里除了ping pong，由于处理等待kik响应时间较长可能 导致tcp读缓冲区累积过多 导致发送端阻塞 ，所以所有消息应该异步排队处理并响应， ping pong应该是需要异步但是不排队
    let frame = Frame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    match frame.clone() {
        Frame::Cmd(req) => {
            let (cmd_id, cmd_options, cmd) = req.split();
            debug!("handel ctrl cmd:{:?}",cmd);
            //保证方法结束时 set none cmd_id了,这里判断一下目前流程来说其实一般没用，除非ctrl连接重连并快速发命令
            if !context.set_now_cmd_id_if_none(cmd_id.clone()).await {
                channel
                    .clone()
                    .lock()
                    .await
                    .write_and_flush(&ctrl_server_resp_error(
                        cmd_id,
                        format!("命令执行中，不可执行其他命令,cmd:{:?}", cmd),
                    ))
                    .await?;
                return Ok(());
            };

            let context_c = context.clone();
            let f = move || {
                let context = context_c;
                Box::pin(async move {
                    match cmd {
                        Command::Sys(sys) => match sys {
                            SysCommand::List => {
                                let can_ctrl_kiks = context.get_can_ctrl_kik().await;
                                let resp = if can_ctrl_kiks.is_empty() {
                                    ctrl_server_resp_error(cmd_id, "没有可控制的Kik".to_owned())
                                } else {
                                    let mut info = String::new();
                                    for (id, kik) in can_ctrl_kiks.iter() {
                                        info +=
                                            format!("{}--->{}\n", id, kik.kik_info.name).as_str();
                                    }
                                    ctrl_server_resp_success(cmd_id, info)
                                };
                                channel.clone().lock().await.write_and_flush(&resp).await?;
                            }
                            SysCommand::Use(id) => {
                                let op = context.get_initialized_kik_by_id(id.as_str()).await;
                                let resp = if op.is_some() {
                                    let choose_kik = op.unwrap();
                                    if choose_kik.exist_kik_conn().await {
                                        context.set_kik(choose_kik.clone()).await;
                                        ctrl_server_resp_success(
                                            cmd_id,
                                            format!(
                                                "您正在控制 {}-----{}",
                                                choose_kik.kik_info.name, id
                                            ),
                                        )
                                    } else {
                                        ctrl_server_resp_error(
                                            cmd_id,
                                            format!("id为{}的Kik已下线", id),
                                        )
                                    }
                                } else {
                                    ctrl_server_resp_error(cmd_id, format!("找不到id为{}的Kik", id))
                                };
                                channel.clone().lock().await.write_and_flush(&resp).await?;
                            }
                            SysCommand::Now => {
                                let info = match context.get_kik().await {
                                    None => "没有正在控制的Kik".to_string(),
                                    Some(kik) => {
                                        if kik.exist_kik_conn().await {
                                            format!(
                                                "当前正在控制 {}-----{}",
                                                kik.kik_info.name,
                                                kik.kik_info.id.unwrap()
                                            )
                                        } else {
                                            "被控制的kik已下线".to_string()
                                        }
                                    }
                                };
                                channel
                                    .lock()
                                    .await
                                    .write_and_flush(&ctrl_server_resp_success(cmd_id, info))
                                    .await?;
                            }
                        },
                        Command::Local(_) => {
                            return Err(default_error());
                        }
                        //除了以上 类型，下面的需要kik执行并响应
                        cmd => {
                            match context.get_kik().await {
                                None => {
                                    let info = "没有被控制的Kik！".to_string();
                                    channel
                                        .clone()
                                        .lock()
                                        .await
                                        .write_and_flush(&ctrl_server_resp_error(cmd_id, info))
                                        .await?;
                                }
                                Some(kik) => {
                                    //todo 稍微优化代码
                                    match kik.get_kik_conn().await {
                                        None => {
                                            let info = "被控制的Kik已下线".to_string();
                                            channel
                                                .clone()
                                                .lock()
                                                .await
                                                .write_and_flush(&ctrl_server_resp_error(
                                                    cmd_id, info,
                                                ))
                                                .await?;
                                        }
                                        Some(kik_conn) => {
                                            kik_conn
                                                .clone()
                                                .lock()
                                                .await
                                                .try_write_and_flush(
                                                    &protocol::transfer_encode_frame(
                                                        KikFrame::Cmd(ReqCmd::new(
                                                            cmd_id.clone(),
                                                            cmd_options.clone(),
                                                            cmd.clone(),
                                                        )),
                                                    ),
                                                )
                                                .await;

                                            let rx_arc =
                                                kik_conn
                                                    .lock()
                                                    .await
                                                    .get::<Arc<
                                                        Mutex<
                                                            Receiver<(kik_resp::KikResp, String)>,
                                                        >,
                                                    >>(
                                                        "rx"
                                                    )
                                                    .expect("不可能没有rx")
                                                    .clone();

                                            let mut resp_op = None;

                                            //正常数据超时等待时间为5分钟

                                            let mut duration = Duration::from_secs(60 * 5);
                                            for _ in 0..3 {
                                                let res = if cmd_options.timeout() {
                                                    timeout(
                                                        duration,
                                                        rx_arc.clone().lock().await.recv(),
                                                    )
                                                    .await
                                                } else {
                                                    Ok(rx_arc.clone().lock().await.recv().await)
                                                };

                                                match res {
                                                    Ok(Some((resp, resp_cmd_id))) => {
                                                        if resp_cmd_id != cmd_id {
                                                            //虽然send前判断了过期id不再来，但是可能判断后发生了超时修改了id再发，这里依然拿到了过期响应，于是尝试重读下一个
                                                            //只有这里continue,因为只有这里重试读,并且这次读等的时间短一点
                                                            //极其偶然需要记录日志
                                                            warn!("得到过期响应或异常响应");
                                                            duration = duration / 2;
                                                            continue;
                                                        } else {
                                                            resp_op = Some(ctrl_kik_resp(
                                                                cmd_id.clone(),
                                                                resp,
                                                            ));
                                                        }
                                                    }
                                                    Ok(None) => {
                                                        //写端关闭，其实这是不可能的,因为channel_arc还在
                                                        resp_op = Some(ctrl_server_resp_error(
                                                            cmd_id.clone(),
                                                            "被控端下线".to_string(),
                                                        ));
                                                    }
                                                    Err(_) => {
                                                        //超时
                                                        resp_op = Some(ctrl_server_resp_error(
                                                            cmd_id.clone(),
                                                            "被控端执行命令超时".to_string(),
                                                        ));
                                                    }
                                                };

                                                break;
                                            }

                                            if resp_op.is_some() {
                                                channel
                                                    .clone()
                                                    .lock()
                                                    .await
                                                    .write_and_flush(&resp_op.unwrap())
                                                    .await?;
                                            } else {
                                                //说明三次都读的过期或异常数据，有问题，放弃这个kik
                                                //日志报告
                                                context
                                                    .offline_kik(kik.kik_info.id.unwrap().as_str())
                                                    .await;
                                                channel
                                                    .clone()
                                                    .lock()
                                                    .await
                                                    .write_and_flush(&ctrl_server_resp_error(
                                                        cmd_id,
                                                        "被控者不对劲，已强制其下线".to_string(),
                                                    ))
                                                    .await?;
                                            }
                                        }
                                    };
                                }
                            };
                        }
                    }
                    Ok(())
                })
            };
            let r: anyhow::Result<()> = f().await;
            context.delete_now_cmd_id().await;
            return r;
        }
        Frame::Ping => {}
        Frame::Pong => {}
        _ => {
            return Err(default_error());
        }
    };
    Ok(())
}

pub async fn handle_ctrl_data(
    context: Context,
    _: Arc<Mutex<Channel>>,
    msg: BytesMut,
) -> anyhow::Result<()> {
    let frame = Frame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    match frame {
        Frame::Data(id, data) => {
            // let async_executor = async_util::new(5);
            //todo 用这个有生命周期问题，搞不懂
            // async_executor.submit(Box::new(move || {
            //     Box::pin(async {})
            // })).await;

            //todo 优化为全局单线程池
            //todo 学习该项目源码完善async_executor
            let thread_pool = ThreadPool::new(ThreadPoolConfig {
                worker_threads: 1,
                queue_capacity: 1,
            });

            //目前这种方式可能会比较消耗服务器内存
            let len = data.len();
            debug!("开始发送长度为{}的数据", len);
            let task = Task::new(
                async move {
                    //ctrl data过来，如果kik不在线，就不管
                    if let Some(kik) = context.get_kik().await {
                        //如果未找到kik的data_conn，也不管
                        if let Some(data_c) = kik.find_data_conn().await {
                            data_c
                                .lock()
                                .await
                                .try_write_and_flush(&protocol::transfer_encode_frame(
                                    KikFrame::Data(id, data)
                                ))
                                .await;
                        }
                    }
                    Ok(())
                },
                1,
            );
            _ = thread_pool.submit(task).await?;
            debug!("长度为{}的数据发送完毕", len);
            // let result = handle.await_result().await;
        }
        Frame::Ping => {}
        Frame::Pong => {}
        _ => {
            return Err(default_error());
        }
    }
    Ok(())
}

pub async fn handle_kik_data(
    context: Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
) -> anyhow::Result<()> {
    let frame = KikFrame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    match frame {
        KikFrame::Data(id, data) => {
            match context.find_ctrl_data().await {
                None => {
                    //未找到ctrl的data_conn或者根本没有ctrl,不管，
                }
                Some(c) => {
                    c.lock()
                        .await
                        .try_write_and_flush(&protocol::transfer_encode_frame(Frame::Data(
                            id, data,
                        )))
                        .await;
                }
            }
        }

        KikFrame::Ping => {}
        KikFrame::Pong => {}
        _ => {
            return Err(default_error());
        }
    }
    Ok(())
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
                .get::<Sender<(kik_resp::KikResp, String)>>("tx")
                .expect("不可能没有tx")
                .clone();
            match context.now_cmd_id().await {
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
            tx.send((resp, cmd_id)).await.expect("读端不可能关闭");
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
pub async fn handle_init_message(
    config: Config,
    context: Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
) -> anyhow::Result<()> {
    let frame = InitFrame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    channel
        .clone()
        .lock()
        .await
        .set_id(Uuid::new_v4().to_string());
    //初始化id
    debug!("init frame:{:?}", frame);
    match frame {
        InitFrame::CtrlAuthReq(s) => {
            if s == config.id.encrypt() {
                let auth = ctrl_auth_success(&context, &channel).await;
                //最后再允许发ping,这里之后要有一定延时才能发ping
                // 一定要保证这里无论如何会设置状态， 因为如果?返回了错误 需要依赖状态去清理
                channel.lock().await.channel_type = ChannelType::Ctrl; //代表 可向这个连接发ping了 且 后续发到此连接的消息都会被当做业务消息处理
                auth?;
            } else {
                channel
                    .lock()
                    .await
                    .write_and_flush(&protocol::transfer_encode_frame(InitFrame::CtrlAuthReply(
                        false,
                    )))
                    .await?;
                // time::sleep(Duration::from_secs(2)).await;
                return Err(anyhow::Error::msg("校验失败"));
            }
        }
        InitFrame::CtrlDataConnReq(s) => {
            //todo 解析密文s，获取客户端传来时间戳并校验，可防止中间人
            if s == config.id.encrypt() {
                if !context.exist_ctrl().await {
                    return Err(anyhow::Error::msg("没有此控制者，或者该控制连接已断开"));
                }
                let auth = ctrl_data_auth_success(&context, &channel).await;
                channel.lock().await.channel_type = ChannelType::CtrlData;
                auth?;
            } else {
                channel
                    .lock()
                    .await
                    .write_and_flush(&protocol::transfer_encode_frame(
                        InitFrame::CtrlDataConnAuthReply(false),
                    ))
                    .await?;
                // time::sleep(Duration::from_secs(2)).await;
                return Err(anyhow::Error::msg("数据连接校验失败"));
            }
        }

        InitFrame::KikReq(kik_info) => {
            let ok = kik_req(&context, &channel, kik_info).await;
            channel.lock().await.channel_type = ChannelType::Kik;
            ok?;
        }
        InitFrame::KikDataConnReq(id) => {
            // kikdata 连接的 kik_id在attr中
            let ok = kik_data_req(context, &channel, id).await;
            channel.lock().await.channel_type = ChannelType::KikData;
            ok?
        }
        _ => {
            return Err(default_error());
        }
    }
    Ok(())
}

async fn kik_data_req(
    context: Context,
    channel: &Arc<Mutex<Channel>>,
    id: String,
) -> anyhow::Result<()> {
    channel.lock().await.put("kik_id".to_string(), id.clone());
    
    //未初始化完成的kik也可以添加kik_data_conn
    match context.kik_map.clone().read().await.get(&id) {
        None => {
            return Err(anyhow::Error::msg("没有这个被控者却来了该被控者连接"));
        }
        Some(kik) => {
            kik.insert_data_conn(channel.clone()).await;
            //将 kik id返回表示成功
            channel
                .lock()
                .await
                .write_and_flush(&protocol::transfer_encode_frame(InitFrame::KikId(id)))
                .await?;
        }
    };
    Ok(())
}

async fn kik_req(
    context: &Context,
    channel: &Arc<Mutex<Channel>>,
    mut kik_info: KikInfo,
) -> anyhow::Result<()> {
    let kik = match kik_info.clone().id {
        None => {
            let id;
            {
                let arc = channel.clone();
                let guard = arc.lock().await;
                id = guard.get_id().to_string();
                kik_info.id = Some(id.clone());
            }
            //先响应确认和分配内存，但是上线延迟
            let kik = new_kik_login_line(context, channel, &kik_info, id).await;
            channel.lock().await.write_and_flush(&protocol::transfer_encode_frame(InitFrame::KikId(
                kik.kik_info.id.clone().unwrap(),
            ))).await?;
            kik
        }
        //重连
        Some(id) => {
            {
                let arc = channel.clone();
                let mut guard = arc.lock().await;
                //用人家带过来的id，覆盖自动生成的
                guard.set_id(id.clone());
            }

            //先响应确认和分配内存，但是上线延迟
            let kik = kik_reconnect_line(context, channel, &kik_info, &id).await;

            channel.lock().await.write_and_flush(&protocol::transfer_encode_frame(InitFrame::KikId(
                id.clone(),
            ))).await?;

            kik
        }
    };

    //先响应确认和分配内存，但是上线延迟(等待kik数据连接等状态准备好)
    tokio::time::sleep(Duration::from_secs(5)).await;
    //初始化完成，即kik上线
    kik.set_kik_initialized(true);
    //没有当前被控者，默认设置一个
    let current = match context.get_kik().await {
        None => false,
        Some(kik) => kik.exist_kik_conn().await,
    };
    if !current {
        context.set_kik(kik.clone()).await;
    }

    //这个用于传输kik的响应
    let (tx, rx) = mpsc::channel::<(kik_resp::KikResp, String)>(5);
    {
        let arc = channel.clone();
        let mut mutex_guard = arc.lock().await;
        mutex_guard.put("rx".to_string(), Arc::new(Mutex::new(rx)));
        mutex_guard.put("tx".to_string(), tx);
    }
    Ok(())
}

async fn kik_reconnect_line(
    context: &Context,
    channel: &Arc<Mutex<Channel>>,
    kik_info: &KikInfo,
    id: &String,
) -> Kik {
 
    info!(
        "【{}】重连，ip:{}",
        kik_info.name,
        channel
            .lock()
            .await
            .get_peer_addr()
            .as_ref()
            .map(|addr| addr.to_string())
            .unwrap_or("未知ip".to_string())
    );
    let arc = context.kik_map.clone();
    let mut kik_map = arc.write().await;
    match kik_map.get(id) {
        None => {
            // 重连发现以前的kik从map中删除，那么插入
            let kik = Kik::new(id.as_str(), kik_info.name.as_str(), channel.clone());
            kik_map.insert(id.clone(), kik.clone());
            kik
        }
        Some(kik) => {
            //有旧的连接就删了
            match kik.set_kik_conn(channel.clone()).await {
                None => {}
                Some(old) => {
                    old.lock().await.try_write_half_close().await;
                }
            }
            kik.clone()
        }
    }
}


//kik上线，代表控制端可以向其发送业务消息
async fn new_kik_login_line(
    context: &Context,
    channel: &Arc<Mutex<Channel>>,
    kik_info: &KikInfo,
    id: String,
) -> Kik {
    //将自动生成的id返回做为Kik id
    let kik = Kik::new(id.as_str(), kik_info.name.as_str(), channel.clone());
    context
        .kik_map
        .write()
        .await
        .insert(id.clone(), kik.clone());
    info!(
        "【{}】上线，ip:{}",
        kik_info.name,
        channel
            .lock()
            .await
            .get_peer_addr()
            .as_ref()
            .map(|addr| addr.to_string())
            .unwrap_or("未知ip".to_string())
    );
    push_event().await;
    kik
}

async fn ctrl_data_auth_success(
    context: &Context,
    channel: &Arc<Mutex<Channel>>,
) -> anyhow::Result<()> {
    context.insert_ctrl_data_conn(channel.clone()).await;
    //写回一个ctrl data 连接校验的确认帧
    channel
        .lock()
        .await
        .write_and_flush(&protocol::transfer_encode_frame(
            InitFrame::CtrlDataConnAuthReply(true),
        ))
        .await?;
    Ok(())
}

async fn ctrl_auth_success(context: &Context, channel: &Arc<Mutex<Channel>>) -> anyhow::Result<()> {
    match context.set_ctrl_conn(channel.clone()).await {
        None => {}
        Some(old) => {
            // 旧的ctrl关掉
            old.lock().await.try_write_half_close().await;
            context.clear_all_ctrl_data().await;
        }
    }; //代表有控制者了
       //写回一个ctrl连接校验确认帧
    channel
        .lock()
        .await
        .write_and_flush(&protocol::transfer_encode_frame(InitFrame::CtrlAuthReply(
            true,
        )))
        .await?;
    Ok(())
}

async fn push_event() {
    //todo 上线事件
}
