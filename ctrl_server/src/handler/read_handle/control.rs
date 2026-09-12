use crate::core::connection_meta::KIK_RESPONSE_RX;
use crate::core::context::Context;
use bytes::BytesMut;
use common::channel::Channel;
use common::command::{Command, SysCommand};
use common::file_util::LONG_COMMAND_TIMEOUT;
use common::message::kik_frame::KikFrame;
use common::protocol::{self, BufSerializable, ReqCmd};
use ctrl_common::cmd_resp_info::{KikInfoVo, SysNow};
use ctrl_common::ctrl_frame::Frame;
use ctrl_common::ctrl_protocol::{ctrl_kik_resp, ctrl_server_resp_error, ctrl_server_resp_success};
use futures::stream;
use log::{debug, warn};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::{timeout_at, Instant};
use tokio_stream::StreamExt;
fn default_error() -> anyhow::Error {
    anyhow::Error::msg("不支持的帧类型")
}

pub async fn handle_ctrl(
    context: Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
    allow_remote_exec: bool,
) -> anyhow::Result<()> {
    // 当前协议明确只允许一个活动命令，因此业务处理在控制连接读循环内串行等待；
    // 客户端命令门禁和服务端 active_command_id 共同阻止无界命令排队。
    let frame = Frame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    match frame {
        Frame::Cmd(req) => {
            let (cmd_id, cmd_options, cmd) = req.split();
            if matches!(&cmd, Command::Exec(_)) && !allow_remote_exec {
                channel
                    .lock()
                    .await
                    .write_and_flush(&ctrl_server_resp_error(
                        cmd_id,
                        "服务端当前策略禁止 Exec，可通过 CTRL_SERVER_ALLOW_EXEC=1 覆盖".to_string(),
                    ))
                    .await?;
                return Ok(());
            }
            debug!("handel ctrl cmd:{:?}", cmd);
            //保证方法结束时 set none cmd_id了,这里判断一下目前流程来说其实一般没用，除非ctrl连接重连并快速发命令
            if !context.try_begin_command(cmd_id.clone()).await {
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
                                    let list: Vec<KikInfoVo> =
                                        stream::iter(can_ctrl_kiks.into_iter())
                                            .then(|(id, k)| async move {
                                                KikInfoVo {
                                                    id,
                                                    name: k.kik_client_info.kik_info.name,
                                                    ip: k
                                                        .kik_client_info
                                                        .ip
                                                        .read()
                                                        .await
                                                        .to_string(),
                                                    recent_online_time: *k
                                                        .kik_client_info
                                                        .recent_online_time
                                                        .read()
                                                        .await,
                                                }
                                            })
                                            .collect()
                                            .await;
                                    ctrl_server_resp_success(cmd_id, serde_json::to_string(&list)?)
                                };
                                channel.clone().lock().await.write_and_flush(&resp).await?;
                            }
                            SysCommand::Use(id) => {
                                let op = context.get_initialized_kik_by_id(id.as_str()).await;
                                let resp = if let Some(choose_kik) = op {
                                    if choose_kik.exist_kik_conn().await {
                                        context.set_kik(choose_kik.clone()).await;
                                        ctrl_server_resp_success(
                                            cmd_id,
                                            serde_json::to_string(&KikInfoVo {
                                                id,
                                                name: choose_kik.kik_client_info.kik_info.name,
                                                ip: choose_kik
                                                    .kik_client_info
                                                    .ip
                                                    .read()
                                                    .await
                                                    .clone(),
                                                recent_online_time: *choose_kik
                                                    .kik_client_info
                                                    .recent_online_time
                                                    .read()
                                                    .await,
                                            })?,
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
                                    None => SysNow::None, //"没有正在控制的Kik"
                                    Some(kik) => {
                                        if kik.exist_kik_conn().await {
                                            SysNow::Kik(KikInfoVo {
                                                id: kik
                                                    .id()
                                                    .ok_or_else(|| {
                                                        anyhow::anyhow!("已注册 Kik 缺少实例 ID")
                                                    })?
                                                    .to_string(),
                                                name: kik.kik_client_info.kik_info.name,
                                                ip: kik.kik_client_info.ip.read().await.clone(),
                                                recent_online_time: *kik
                                                    .kik_client_info
                                                    .recent_online_time
                                                    .read()
                                                    .await,
                                            })
                                        } else {
                                            SysNow::NotOnline //"被控制的kik已下线".to_string()
                                        }
                                    }
                                };
                                channel
                                    .lock()
                                    .await
                                    .write_and_flush(&ctrl_server_resp_success(
                                        cmd_id,
                                        serde_json::to_string(&info)?,
                                    ))
                                    .await?;
                            }
                            SysCommand::History(kik_id) => {
                                let records = context.kik_presence(kik_id.as_deref()).await;
                                let response = if kik_id.is_some() && records.is_empty() {
                                    ctrl_server_resp_error(
                                        cmd_id,
                                        "找不到该 Kik 的上下线记录".into(),
                                    )
                                } else {
                                    ctrl_server_resp_success(
                                        cmd_id,
                                        serde_json::to_string(&records)?,
                                    )
                                };
                                channel.lock().await.write_and_flush(&response).await?;
                            }
                        },
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
                                            let command_timeout = cmd_options.timeout();
                                            let request =
                                                protocol::transfer_encode_frame(KikFrame::Cmd(
                                                    ReqCmd::new(cmd_id.clone(), cmd_options, cmd),
                                                ));
                                            if let Err(error) = kik_conn
                                                .lock()
                                                .await
                                                .write_and_flush(&request)
                                                .await
                                            {
                                                warn!("向被控端写入控制命令失败: {error}");
                                                channel
                                                    .lock()
                                                    .await
                                                    .write_and_flush(&ctrl_server_resp_error(
                                                        cmd_id,
                                                        "向被控端发送命令失败".to_string(),
                                                    ))
                                                    .await?;
                                                return Ok(());
                                            }

                                            let rx_arc = kik_conn
                                                .lock()
                                                .await
                                                .attribute(&KIK_RESPONSE_RX)
                                                .cloned()
                                                .ok_or_else(|| {
                                                    anyhow::anyhow!("被控端响应队列未初始化")
                                                })?;

                                            let mut resp_op = None;

                                            let response_timeout = if command_timeout {
                                                Duration::from_secs(60 * 5)
                                            } else {
                                                LONG_COMMAND_TIMEOUT
                                            };
                                            let deadline = Instant::now() + response_timeout;
                                            for _ in 0..3 {
                                                let res = timeout_at(deadline, async {
                                                    rx_arc.lock().await.recv().await
                                                })
                                                .await;

                                                match res {
                                                    Ok(Some((resp, resp_cmd_id))) => {
                                                        if resp_cmd_id != cmd_id {
                                                            //虽然send前判断了过期id不再来，但是可能判断后发生了超时修改了id再发，这里依然拿到了过期响应，于是尝试重读下一个
                                                            //只有这里continue,因为只有这里重试读,并且这次读等的时间短一点
                                                            //极其偶然需要记录日志
                                                            warn!("得到过期响应或异常响应");
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

                                            if let Some(response) = resp_op {
                                                channel
                                                    .clone()
                                                    .lock()
                                                    .await
                                                    .write_and_flush(&response)
                                                    .await?;
                                            } else {
                                                //说明三次都读的过期或异常数据，有问题，放弃这个kik
                                                //日志报告
                                                let kik_id = kik.id().ok_or_else(|| {
                                                    anyhow::anyhow!("已注册 Kik 缺少实例 ID")
                                                })?;
                                                context.offline_kik(kik_id).await;
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
            context.finish_command().await;
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
