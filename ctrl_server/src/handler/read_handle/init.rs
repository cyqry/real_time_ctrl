use crate::core::connection_meta::{
    CTRL_AUTH_CLIENT_NONCE, CTRL_AUTH_SERVER_NONCE, KIK_ID, KIK_RESPONSE_RX, KIK_RESPONSE_TX,
};
use crate::core::context::Context;
use bytes::BytesMut;
use common::channel::{Channel, ChannelType};
use common::config::Config;
use common::kik_info::KikInfo;
use common::message::init_frame::InitFrame;
use common::message::kik_resp;
use common::protocol::{self, BufSerializable};
use common::session_auth::{
    is_valid_nonce_hex, random_nonce_hex, verify_ctrl_auth_proof, verify_ctrl_data_proof,
};
use ctrl_common::kik::Kik;
use log::{debug, info, warn};
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::{mpsc, Mutex};
use uuid::Uuid;

fn default_error() -> anyhow::Error {
    anyhow::Error::msg("不支持的初始化帧类型")
}
pub async fn handle_init_message(
    config: Config,
    context: Context,
    channel: Arc<Mutex<Channel>>,
    msg: BytesMut,
    allow_ctrl: bool,
    allow_kik: bool,
) -> anyhow::Result<()> {
    let frame = InitFrame::from_buf(msg).ok_or(anyhow::Error::msg("帧格式错误"))?;
    let is_ctrl_frame = matches!(
        &frame,
        InitFrame::CtrlAuthStart(_)
            | InitFrame::CtrlAuthProof { .. }
            | InitFrame::CtrlDataSessionReq { .. }
    );
    let is_kik_frame = matches!(&frame, InitFrame::KikReq(_) | InitFrame::KikDataConnReq(_));
    if (is_ctrl_frame && !allow_ctrl) || (is_kik_frame && !allow_kik) {
        warn!("连接在不允许的传输端口声明角色，已拒绝");
        return Err(anyhow::Error::msg("当前传输端口不允许该连接角色"));
    }
    // challenge/proof 会分两帧进入这里，连接 ID 只在第一帧生成一次，避免握手中途身份漂移。
    let mut channel_guard = channel.lock().await;
    if channel_guard.id().is_none() {
        channel_guard.set_id(Uuid::new_v4().to_string());
    }
    drop(channel_guard);
    //初始化id
    debug!("init frame:{:?}", frame);
    match frame {
        InitFrame::CtrlAuthStart(client_nonce) => {
            if !is_valid_nonce_hex(&client_nonce) {
                return Err(anyhow::Error::msg("控制端 client nonce 格式错误"));
            }
            e2e_trace("server: received ctrl auth start");
            let server_nonce = random_nonce_hex();
            {
                let mut guard = channel.lock().await;
                guard.insert_attribute(&CTRL_AUTH_CLIENT_NONCE, client_nonce);
                guard.insert_attribute(&CTRL_AUTH_SERVER_NONCE, server_nonce.clone());
            }
            channel
                .lock()
                .await
                .write_and_flush(&protocol::transfer_encode_frame(
                    InitFrame::CtrlAuthChallenge(server_nonce),
                ))
                .await?;
            e2e_trace("server: sent ctrl auth challenge");
        }
        InitFrame::CtrlAuthProof {
            client_nonce,
            proof,
        } => {
            e2e_trace("server: received ctrl auth proof");
            let (expected_client_nonce, server_nonce) = {
                let guard = channel.lock().await;
                let expected_client_nonce = guard
                    .attribute(&CTRL_AUTH_CLIENT_NONCE)
                    .cloned()
                    .ok_or(anyhow::Error::msg("缺少控制端认证 client nonce"))?;
                let server_nonce = guard
                    .attribute(&CTRL_AUTH_SERVER_NONCE)
                    .cloned()
                    .ok_or(anyhow::Error::msg("缺少控制端认证 server nonce"))?;
                (expected_client_nonce, server_nonce)
            };

            let secret = config.id.control_plane_secret();
            if expected_client_nonce == client_nonce
                && verify_ctrl_auth_proof(secret, &client_nonce, &server_nonce, &proof)
            {
                e2e_trace("server: ctrl auth proof verified");
                let session_id = random_nonce_hex();
                let auth = complete_ctrl_auth(&context, &channel, session_id).await;
                channel.lock().await.channel_type = ChannelType::Ctrl;
                auth?;
                e2e_trace("server: sent ctrl auth session");
            } else {
                e2e_trace("server: ctrl auth proof rejected");
                return Err(anyhow::Error::msg("控制连接校验失败"));
            }
        }
        InitFrame::CtrlDataSessionReq {
            session_id,
            channel_nonce,
            proof,
        } => {
            e2e_trace("server: received ctrl data session req");
            if !is_valid_nonce_hex(&session_id) || !is_valid_nonce_hex(&channel_nonce) {
                return Err(anyhow::Error::msg("数据通道会话或 nonce 格式错误"));
            }
            let secret = config.id.control_plane_secret();
            let proof_ok = verify_ctrl_data_proof(secret, &session_id, &channel_nonce, &proof);
            let session_ok = if proof_ok {
                context
                    .validate_ctrl_data_session(&session_id, &channel_nonce)
                    .await
            } else {
                false
            };
            if session_ok {
                e2e_trace("server: ctrl data session verified");
                let auth = complete_ctrl_data_auth(&context, &channel).await;
                channel.lock().await.channel_type = ChannelType::CtrlData;
                auth?;
                e2e_trace("server: sent ctrl data session reply");
            } else {
                e2e_trace("server: ctrl data session rejected");
                channel
                    .lock()
                    .await
                    .write_and_flush(&protocol::transfer_encode_frame(
                        InitFrame::CtrlDataSessionReply(false),
                    ))
                    .await?;
                return Err(anyhow::Error::msg("数据连接会话绑定失败"));
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
    channel.lock().await.insert_attribute(&KIK_ID, id.clone());

    //未初始化完成的kik也可以添加kik_data_conn
    let kik = context
        .kiks
        .read()
        .await
        .get(&id)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("没有这个被控者却收到了该被控端数据连接"))?;
    if !kik.insert_data_conn(channel.clone()).await {
        return Err(anyhow::Error::msg("被控端数据通道达到上限"));
    }
    // 将 kik id 返回表示成功。
    channel
        .lock()
        .await
        .write_and_flush(&protocol::transfer_encode_frame(InitFrame::KikId(id)))
        .await?;
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
                id = guard.require_id()?.to_string();
                kik_info.id = Some(id.clone());
            }
            //先响应确认和分配内存，但是上线延迟
            let assigned_id = id.clone();
            let kik = new_kik_login_line(context, channel, &kik_info, id).await;
            channel
                .lock()
                .await
                .write_and_flush(&protocol::transfer_encode_frame(InitFrame::KikId(
                    assigned_id,
                )))
                .await?;
            kik
        }
        //重连
        Some(id) => {
            // Kik ID 只能来自服务端首次分配的 UUID。它不是认证凭据，但限制格式可避免
            // 匿名输入污染索引、日志和管理面 JSON，并让历史响应大小保持可计算。
            Uuid::parse_str(&id).map_err(|_| anyhow::anyhow!("Kik 重连 ID 格式无效"))?;
            {
                let arc = channel.clone();
                let mut guard = arc.lock().await;
                //用人家带过来的id，覆盖自动生成的
                guard.set_id(id.clone());
            }

            //先响应确认和分配内存，但是上线延迟
            let kik = kik_reconnect_line(context, channel, &kik_info, &id).await;

            channel
                .lock()
                .await
                .write_and_flush(&protocol::transfer_encode_frame(InitFrame::KikId(
                    id.clone(),
                )))
                .await?;

            kik
        }
    };

    // 响应队列必须先于 initialized 发布，避免控制线程观察到“已上线”却取不到 rx/tx。
    let (tx, rx) = mpsc::channel::<(kik_resp::KikResp, String)>(5);
    {
        let mut channel = channel.lock().await;
        channel.insert_attribute(&KIK_RESPONSE_RX, Arc::new(Mutex::new(rx)));
        channel.insert_attribute(&KIK_RESPONSE_TX, tx);
    }

    //先响应确认和分配内存，但是上线延迟(等待kik数据连接等状态准备好)
    tokio::time::sleep(Duration::from_secs(5)).await;
    //初始化完成，即kik上线
    kik.set_kik_initialized(true);
    context.record_kik_online(&kik).await;
    //没有当前被控者，默认设置一个
    let current = match context.get_kik().await {
        None => false,
        Some(kik) => kik.exist_kik_conn().await,
    };
    if !current {
        context.set_kik(kik.clone()).await;
    }

    Ok(())
}

async fn kik_reconnect_line(
    context: &Context,
    channel: &Arc<Mutex<Channel>>,
    kik_info: &KikInfo,
    id: &str,
) -> Kik {
    let ip = channel
        .lock()
        .await
        .get_peer_addr()
        .as_ref()
        .map(|addr| addr.ip().to_string())
        .unwrap_or("未知ip".to_string());
    info!("【{}】重连，ip:{}", kik_info.name, ip);
    let (kik, existed) = {
        use std::collections::hash_map::Entry;

        let mut kik_map = context.kiks.write().await;
        match kik_map.entry(id.to_owned()) {
            Entry::Vacant(entry) => {
                let kik = Kik::new(
                    id,
                    kik_info.name.as_str(),
                    ip.clone(),
                    SystemTime::now(),
                    channel.clone(),
                );
                entry.insert(kik.clone());
                (kik, false)
            }
            Entry::Occupied(entry) => (entry.get().clone(), true),
        }
    };
    if existed {
        *kik.kik_client_info.ip.write().await = ip;
        *kik.kik_client_info.recent_online_time.write().await = SystemTime::now();
        if let Some(old) = kik.set_kik_conn(channel.clone()).await {
            old.lock().await.try_write_half_close().await;
        }
    }
    kik
}

//kik上线，代表控制端可以向其发送业务消息
async fn new_kik_login_line(
    context: &Context,
    channel: &Arc<Mutex<Channel>>,
    kik_info: &KikInfo,
    id: String,
) -> Kik {
    let ip = channel
        .lock()
        .await
        .get_peer_addr()
        .as_ref()
        .map(|addr| addr.ip().to_string())
        .unwrap_or("未知ip".to_string());
    //将自动生成的id返回做为Kik id
    let kik = Kik::new(
        id.as_str(),
        kik_info.name.as_str(),
        ip.clone(),
        SystemTime::now(),
        channel.clone(),
    );
    context.kiks.write().await.insert(id.clone(), kik.clone());
    info!("【{}】上线，ip:{}", kik_info.name, ip);
    kik
}

async fn complete_ctrl_data_auth(
    context: &Context,
    channel: &Arc<Mutex<Channel>>,
) -> anyhow::Result<()> {
    if !context.insert_ctrl_data_conn(channel.clone()).await {
        return Err(anyhow::Error::msg("控制数据通道达到上限或控制会话已离线"));
    }
    channel
        .lock()
        .await
        .write_and_flush(&protocol::transfer_encode_frame(
            InitFrame::CtrlDataSessionReply(true),
        ))
        .await?;
    Ok(())
}

async fn complete_ctrl_auth(
    context: &Context,
    channel: &Arc<Mutex<Channel>>,
    session_id: String,
) -> anyhow::Result<()> {
    context
        .set_ctrl_conn_with_session(channel.clone(), session_id.clone())
        .await;
    channel
        .lock()
        .await
        .write_and_flush(&protocol::transfer_encode_frame(
            InitFrame::CtrlAuthSession(session_id),
        ))
        .await?;
    Ok(())
}

fn e2e_trace(message: &str) {
    let Some(path) = std::env::var("CTRL_SERVER_E2E_TRACE_PATH")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return;
    };

    // 仅 E2E 测试显式开启，用于定位本地握手；生产默认不写任何 trace 文件。
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{message}");
    }
}
