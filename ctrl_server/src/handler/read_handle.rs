use crate::core::context::Context;
use bytes::BytesMut;
use common::channel::{Channel, ChannelType};
use common::command::{Command, CtrlCommand, SysCommand};
use common::config::Config;
use common::kik::Kik;
use common::message::frame::Frame;
use common::message::resp;
use common::message::resp::Resp::{DataId, Info};
use common::protocol;
use common::protocol::BufSerializable;
use log::debug;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{mpsc, Mutex};
use tokio::time;
use tokio::time::timeout;
use uuid::Uuid;
use common::message::resp::Resp;

fn default_error() -> anyhow::Error {
    anyhow::Error::msg("不支持的帧类型")
}

pub async fn handle_ctrl(
    context: &Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
) -> anyhow::Result<()> {
    let frame = Frame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    let cmd_id = Uuid::new_v4().to_string();

    match frame {
        Frame::Cmd(cmd) => {
            debug!("handel ctrl cmd");
            //保证方法结束时 set none cmd_id了,这里判断一下目前流程来说其实一般没用，除非ctrl连接重连并快速发命令
            if !context.set_now_cmd_id_if_none(cmd_id.clone()).await {
                channel
                    .clone()
                    .lock()
                    .await
                    .write_and_flush(&protocol::resp(Info(format!("命令执行中，不可执行其他命令,cmd:{:?}", cmd
                    ))))
                    .await?;
                return Ok(());
            };
            let f = move || {
                Box::pin(async move {
                    match cmd {
                        Command::Sys(sys) => match sys {
                            SysCommand::List => {
                                let mut info = String::new();
                                for (id, kik) in context.kik_map.clone().read().await.iter() {
                                    if kik.exist_kik_conn().await {
                                        info +=
                                            format!("{}--->{}\n", id, kik.kik_info.name).as_str();
                                    }
                                }
                                if info.trim() == "" {
                                    info += "没有可控制的Kik";
                                }
                                channel
                                    .clone()
                                    .lock()
                                    .await
                                    .write_and_flush(&protocol::resp(Info(info.trim().to_owned())))
                                    .await?;
                            }
                            SysCommand::Use(id) => {
                                let mut info = String::new();
                                let kik_map = context.kik_map.clone();
                                let kik_map = kik_map.read().await;
                                let op = kik_map.get(&id);
                                if op.is_some() {
                                    let choose_kik = op.unwrap();
                                    if choose_kik.exist_kik_conn().await {
                                        context.set_kik(choose_kik.clone()).await;
                                        info += format!(
                                            "您正在控制 {}-----{}",
                                            choose_kik.kik_info.name, id
                                        )
                                            .as_str();
                                    } else {
                                        info += format!("id为{}的Kik已下线", id).as_str();
                                    }
                                } else {
                                    info += format!("找不到id为{}的Kik", id).as_str();
                                }
                                channel
                                    .clone()
                                    .lock()
                                    .await
                                    .write_and_flush(&protocol::resp(Info(info)))
                                    .await?;
                            }
                            SysCommand::Now => {
                                let mut info = "没有正在控制的Kik".to_string();

                                match context.get_kik().await {
                                    None => {}
                                    Some(kik) => {
                                        if kik.exist_kik_conn().await {
                                            info = format!(
                                                "当前正在控制 {}-----{}",
                                                kik.kik_info.name,
                                                kik.kik_info.id.unwrap()
                                            );
                                        } else {
                                            info = "被控制的kik已下线".to_string();
                                        }
                                    }
                                }
                                channel
                                    .clone()
                                    .lock()
                                    .await
                                    .write_and_flush(&protocol::resp(Info(info)))
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
                                        .write_and_flush(&protocol::resp(Info(info)))
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
                                                .write_and_flush(&protocol::resp(Info(info)))
                                                .await?;
                                        }
                                        Some(kik_conn) => {
                                            kik_conn
                                                .clone()
                                                .lock()
                                                .await
                                                .try_write_and_flush(&protocol::transfer_encode(
                                                    Frame::CmdExtra(cmd, cmd_id.clone()).to_buf(),
                                                ))
                                                .await;

                                            let rx_arc = kik_conn
                                                .lock()
                                                .await
                                                .get::<Arc<Mutex<Receiver<(resp::Resp, String)>>>>(
                                                    "rx",
                                                )
                                                .expect("不可能没有rx")
                                                .clone();
                                            let mut resp_op = None;

                                            //正常数据超时等待时间为5分钟
                                            let mut duration = Duration::from_secs(60 * 5);
                                            for _ in 0..3 {
                                                match timeout(duration, rx_arc.clone().lock().await.recv()).await {
                                                    Ok(Some((resp, resp_cmd_id))) => {
                                                        if resp_cmd_id != cmd_id {
                                                            //说明过期或异常响应
                                                            //虽然send前判断了过期id不再来，但是可能判断后发生了超时修改了id再发，这里依然拿到了过期响应，于是尝试重读下一个
                                                            //只有这里continue,因为只有这里重试读,并且这次读等的时间短一点
                                                            //todo 极其偶然需要记录日志
                                                            duration = duration / 2;
                                                            continue;
                                                        } else {
                                                            resp_op = Some(resp);
                                                        }
                                                    }
                                                    Ok(None) => {
                                                        //写端关闭，其实这是不可能的,因为channel_arc还在
                                                        resp_op = Some(Resp::Info("被控端下线".to_string()));
                                                    }
                                                    Err(_) => {
                                                        //超时
                                                        resp_op = Some(Resp::Info("被控端执行命令超时".to_string()));
                                                    }
                                                };
                                                break;
                                            }
                                            if resp_op.is_some() {
                                                channel.clone().lock().await
                                                    .write_and_flush(&protocol::resp(resp_op.unwrap()))
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
                                                    .write_and_flush(&protocol::resp(Info(
                                                        "被控者不对劲，已强制其下线".to_string(),
                                                    )))
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
    context: &Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
) -> anyhow::Result<()> {
    let frame = Frame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    match frame {
        Frame::Data(id, data) => {
            match context.get_kik().await {
                None => {
                    //ctrl data过来，kik不在线，就不管
                }
                Some(kik) => {
                    match kik.find_data_conn().await {
                        None => {
                            //未找到kik的data_conn，也不管
                        }
                        Some(c) => {
                            c.lock()
                                .await
                                .try_write_and_flush(&protocol::transfer_encode(
                                    Frame::Data(id, data).to_buf(),
                                ))
                                .await;
                        }
                    }
                }
            }
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
    context: &Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
) -> anyhow::Result<()> {
    let frame = Frame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    match frame {
        Frame::Data(id, data) => {
            match context.find_ctrl_data().await {
                None => {
                    //未找到ctrl的data_conn或者根本没有ctrl,不管，
                }
                Some(c) => {
                    c.lock()
                        .await
                        .try_write_and_flush(&protocol::transfer_encode(
                            Frame::Data(id, data).to_buf(),
                        ))
                        .await;
                }
            }
        }

        Frame::Ping => {}
        Frame::Pong => {}
        _ => {
            return Err(default_error());
        }
    }
    Ok(())
}

pub async fn handle_kik(
    context: &Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
) -> anyhow::Result<()> {
    let frame = Frame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    match frame {
        Frame::RespExtra(resp, cmd_id) => {
            debug!("handle kik,kik响应:{:?},cmdid:{}", resp, cmd_id);
            let tx = channel
                .lock()
                .await
                .get::<Sender<(resp::Resp, String)>>("tx")
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
        Frame::Ping => {}
        Frame::Pong => {}
        _ => {
            return Err(default_error());
        }
    }
    Ok(())
}

// channel的id在 此方法中初始化
// return err会跳出循环关闭连接
pub async fn handle_init_message(
    config: &Config,
    context: &Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
) -> anyhow::Result<()> {
    let frame = Frame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    channel
        .clone()
        .lock()
        .await
        .set_id(Uuid::new_v4().to_string());
    //初始化id
    debug!("init frame:{:?}", frame);
    match frame {
        Frame::CtrlAuthReq(s) => {
            if s == config.id.encrypt() {
                channel.clone().lock().await.channel_type = ChannelType::Ctrl;
                match context.set_ctrl_conn(channel.clone()).await {
                    None => {}
                    Some(old) => {
                        // 旧的ctrl关掉
                        old.lock().await.try_write_half_close().await;
                    }
                };
                //写回一个ctrl连接校验确认帧
                channel
                    .clone()
                    .lock()
                    .await
                    .write_and_flush(&protocol::transfer_encode(
                        Frame::CtrlAuthReply(true).to_buf(),
                    ))
                    .await?;
            } else {
                channel
                    .clone()
                    .lock()
                    .await
                    .write_and_flush(&protocol::transfer_encode(
                        Frame::CtrlAuthReply(false).to_buf(),
                    ))
                    .await?;
                time::sleep(Duration::from_secs(2)).await;
                return Err(anyhow::Error::msg("校验失败"));
            }
        }
        Frame::CtrlDataConnReq(s) => {
            if s == config.id.encrypt() {
                if context.exist_ctrl().await {
                    context.insert_ctrl_data_conn(channel.clone()).await;
                    channel.clone().lock().await.channel_type = ChannelType::CtrlData;
                    //写回一个ctrl data 连接校验的确认帧
                    channel
                        .clone()
                        .lock()
                        .await
                        .write_and_flush(&protocol::transfer_encode(
                            Frame::CtrlDataConnAuthReply(true).to_buf(),
                        ))
                        .await?;
                } else {
                    return Err(anyhow::Error::msg("没有控制者却来了控制者数据连接"));
                }
            } else {
                channel
                    .clone()
                    .lock()
                    .await
                    .write_and_flush(&protocol::transfer_encode(
                        Frame::CtrlDataConnAuthReply(false).to_buf(),
                    ))
                    .await?;
                time::sleep(Duration::from_secs(2)).await;
                return Err(anyhow::Error::msg("数据连接校验失败"));
            }
        }

        Frame::KikReq(mut kik_info) => {
            let kik = match kik_info.id {
                None => {
                    let id;
                    {
                        let arc = channel.clone();
                        let mut guard = arc.lock().await;
                        id = guard.get_id().to_string();
                        kik_info.id = Some(id.clone());
                        guard.channel_type = ChannelType::Kik;
                    }
                    //将自动生成的id返回做为Kik id
                    let kik = Kik::new(id.as_str(), kik_info.name.as_str(), channel.clone());
                    context
                        .kik_map
                        .clone()
                        .write()
                        .await
                        .insert(id.clone(), kik.clone());
                    kik
                }
                //重连
                Some(id) => {
                    {
                        let arc = channel.clone();
                        let mut guard = arc.lock().await;
                        //用人家带过来的id
                        guard.set_id(id.clone());
                        guard.channel_type = ChannelType::Kik;
                    }
                    let arc = context.kik_map.clone();
                    let mut kik_map = arc.write().await;
                    match kik_map.get(&id) {
                        None => {
                            // 重连发现以前的kik从map中删除，那么插入
                            let kik = Kik::new(id.as_str(), kik_info.name.as_str(), channel.clone());
                            kik_map.insert(id.clone(), kik.clone());
                            kik
                        }
                        Some(kik) => {
                            //有旧的就删了
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
            };

            //没有当前被控者，默认设置一个
            let current = match context.get_kik().await {
                None => {
                    false
                }
                Some(kik) => {
                    kik.exist_kik_conn().await
                }
            };
            if !current {
                context.set_kik(kik.clone()).await;
            }

            let (tx, rx) = mpsc::channel::<(resp::Resp, String)>(5);
            let arc = channel.clone();
            let mut mutex_guard = arc.lock().await;
            mutex_guard.put("rx".to_string(), Arc::new(Mutex::new(rx)));
            mutex_guard.put("tx".to_string(), tx);
            mutex_guard
                .write_and_flush(&protocol::transfer_encode(
                    Frame::KikId(kik.kik_info.id.unwrap()).to_buf(),
                ))
                .await?;
        }
        Frame::KikDataConnReq(id) => {
            // kikdata 连接的 kik_id在attr中
            channel
                .clone()
                .lock()
                .await
                .put("kik_id".to_string(), id.clone());
            match context.kik_map.clone().read().await.get(&id) {
                None => {
                    return Err(anyhow::Error::msg("没有这个被控者却来了该被控者连接"));
                }
                Some(kik) => {
                    kik.insert_data_conn(channel.clone()).await;
                    channel.clone().lock().await.channel_type = ChannelType::KikData;
                    //将 kik id返回表示成功
                    channel
                        .clone()
                        .lock()
                        .await
                        .write_and_flush(&protocol::transfer_encode(Frame::KikId(id).to_buf()))
                        .await?;
                }
            };
        }
        //未初始化的连接是不允许ping pong的
        Frame::Ping => {
            return Err(default_error());
        }
        Frame::Pong => {
            return Err(default_error());
        }
        _ => {
            return Err(default_error());
        }
    }
    Ok(())
}
