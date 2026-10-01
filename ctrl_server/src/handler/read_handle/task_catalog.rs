//! 已认证控制面的任务目录查询。只访问服务器任务目录，不向 Kik 发送任何业务帧。
use crate::{
    core::{connection_meta::CTRL_SESSION_ID, context::Context},
    tasks,
};
use common::channel::Channel;
use ctrl_common::{
    ctrl_protocol::{ctrl_server_resp_error, ctrl_server_resp_success},
    task_catalog::{TaskListPage, TaskListRequest},
};
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::{Mutex, Semaphore},
    time::timeout,
};

/// 所有账号共用四个磁盘扫描名额；普通命令不等待这个门禁，也不会排队积累扫描 future。
static SCANS: Semaphore = Semaphore::const_new(4);
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) async fn handle(
    context: Context,
    channel: Arc<Mutex<Channel>>,
    request: TaskListRequest,
    allow_exec: bool,
) -> anyhow::Result<()> {
    let session = channel
        .lock()
        .await
        .attribute(&CTRL_SESSION_ID)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("控制连接缺少会话绑定"))?;
    let permits = match context.try_acquire_command(&session).await {
        Ok(permits) => permits,
        Err(error) => return respond(&channel, request.id, Err(error)).await,
    };
    if !request.valid() || !allow_exec {
        return respond(
            &channel,
            request.id,
            Err(anyhow::anyhow!("服务端禁止任务执行或列表请求不合法")),
        )
        .await;
    }
    let scan = match SCANS.try_acquire() {
        Ok(permit) => permit,
        Err(_) => {
            return respond(
                &channel,
                request.id,
                Err(anyhow::anyhow!("任务列表查询繁忙，请稍后刷新")),
            )
            .await
        }
    };
    tokio::spawn(async move {
        let _permits = permits;
        let result = {
            let _scan = scan;
            timeout(QUERY_TIMEOUT, async {
                authorize(&context, &session, &request.target).await?;
                let page = tasks::list(&tasks::root()?, &request).await?;
                // 扫描期间会话可能失效或目标下线，返回目录前重新核对同一个明确 ID。
                authorize(&context, &session, &request.target).await?;
                Ok(page)
            })
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("任务列表查询超过 5 秒，请稍后刷新")))
        };
        let _ = respond(&channel, request.id, result).await;
    });
    Ok(())
}

async fn authorize(context: &Context, session: &str, target: &str) -> anyhow::Result<()> {
    let kik = context.get_authorized_target(session, target).await?;
    let connection = kik
        .get_kik_conn()
        .await
        .ok_or_else(|| anyhow::anyhow!("目标 Kik 已下线"))?;
    let connection = connection.lock().await;
    super::task_run::named_task_key(&connection)?;
    Ok(())
}

async fn respond(
    channel: &Arc<Mutex<Channel>>,
    id: String,
    result: anyhow::Result<TaskListPage>,
) -> anyhow::Result<()> {
    let frame = match result {
        Ok(page) => ctrl_server_resp_success(id, serde_json::to_string(&page)?),
        Err(error) => ctrl_server_resp_error(id, error.to_string()),
    };
    channel.lock().await.write_and_flush(&frame).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::account::AccountRegistry;
    use common::{
        channel::ChannelType,
        protocol::BufSerializable,
        task::{TASK_MAIN_KEY, TASK_NAMED_CACHE_CAPABLE},
    };
    use ctrl_common::{
        ctrl_frame::Frame,
        ctrl_resp::{Resp, ServerResp},
        kik::Kik,
    };
    use std::{io, time::SystemTime};
    use tokio::io::{AsyncReadExt, DuplexStream};

    const ALLOWED: &str = "11111111-1111-4111-8111-111111111111";
    const DENIED: &str = "22222222-2222-4222-8222-222222222222";

    fn channel(kind: ChannelType) -> (Arc<Mutex<Channel>>, DuplexStream) {
        let (peer, writer) = tokio::io::duplex(8192);
        (
            Arc::new(Mutex::new(Channel::new(
                Box::pin(writer),
                None,
                kind,
                Err(io::Error::from(io::ErrorKind::NotConnected)),
                Err(io::Error::from(io::ErrorKind::NotConnected)),
            ))),
            peer,
        )
    }

    async fn context() -> (Context, Arc<Mutex<Channel>>, DuplexStream, DuplexStream) {
        let accounts = format!(
            r#"[{{"account_id":"catalog","secret":"catalog-test-secret-0000000000000000","allowed_kiks":["{ALLOWED}"],"max_commands_per_instance":1,"max_commands_per_account":1}}]"#
        );
        let registry =
            AccountRegistry::from_json_or_default(Some(&accounts), String::new()).unwrap();
        let context = Context::init_with_accounts(registry);
        let (controller, peer) = channel(ChannelType::Ctrl);
        context
            .register_ctrl_session(
                controller.clone(),
                "session".into(),
                "catalog".into(),
                "catalog-test".into(),
            )
            .await
            .unwrap();
        let (connection, kik_peer) = channel(ChannelType::Kik);
        let kik = Kik::new(
            ALLOWED,
            "test",
            "127.0.0.1".into(),
            SystemTime::now(),
            connection,
        );
        kik.set_kik_initialized(true);
        context.kiks.write().await.insert(ALLOWED.into(), kik);
        (context, controller, peer, kik_peer)
    }

    async fn error(peer: &mut DuplexStream) -> String {
        let length = timeout(Duration::from_secs(2), peer.read_u32())
            .await
            .unwrap()
            .unwrap();
        let mut bytes = vec![0; length as usize];
        peer.read_exact(&mut bytes).await.unwrap();
        let Some(Frame::Resp(response)) = Frame::from_buf(bytes.as_slice().into()) else {
            panic!("expected response")
        };
        assert_eq!(response.get_cmd_id(), "request");
        let Resp::Server(ServerResp::Error(_, text)) = response.get_resp() else {
            panic!("expected server error")
        };
        text.clone()
    }

    fn request(target: &str) -> TaskListRequest {
        TaskListRequest {
            id: "request".into(),
            target: target.into(),
            query: String::new(),
            cursor: None,
            limit: 32,
        }
    }

    #[tokio::test]
    async fn catalog_authorization_rejects_unknown_session_acl_offline_and_unsupported_kik() {
        let (context, _, _, mut kik_peer) = context().await;
        assert!(authorize(&context, "not-a-session", ALLOWED).await.is_err());
        assert!(authorize(&context, "session", DENIED)
            .await
            .unwrap_err()
            .to_string()
            .contains("无权"));
        assert!(authorize(&context, "session", ALLOWED)
            .await
            .unwrap_err()
            .to_string()
            .contains("不支持"));
        let kik = context.kiks.read().await.get(ALLOWED).unwrap().clone();
        let main = kik.get_kik_conn().await.unwrap();
        main.lock()
            .await
            .insert_attribute(&TASK_MAIN_KEY, [7u8; 32]);
        // 旧任务密钥仍不能列举新文件名任务，必须显式完成新能力握手。
        assert!(authorize(&context, "session", ALLOWED)
            .await
            .unwrap_err()
            .to_string()
            .contains("升级"));
        main.lock()
            .await
            .insert_attribute(&TASK_NAMED_CACHE_CAPABLE, true);
        assert!(authorize(&context, "session", ALLOWED).await.is_ok());
        context.kiks.write().await.remove(ALLOWED);
        assert!(authorize(&context, "session", ALLOWED)
            .await
            .unwrap_err()
            .to_string()
            .contains("下线"));
        assert!(timeout(Duration::from_millis(30), kik_peer.read_u8())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn catalog_policy_and_account_quota_fail_without_touching_kik_or_leaking_permits() {
        let (context, controller, mut peer, mut kik_peer) = context().await;
        super::super::control::handle_ctrl(
            context.clone(),
            controller.clone(),
            Frame::TaskList(request(ALLOWED)).to_buf(),
            false,
        )
        .await
        .unwrap();
        assert!(error(&mut peer).await.contains("禁止"));
        let permit = context.try_acquire_command("session").await.unwrap();
        handle(context.clone(), controller.clone(), request(ALLOWED), true)
            .await
            .unwrap();
        assert!(error(&mut peer).await.contains("并发"));
        drop(permit);
        handle(context.clone(), controller.clone(), request(DENIED), true)
            .await
            .unwrap();
        assert!(error(&mut peer).await.contains("无权"));
        assert!(context.try_acquire_command("session").await.is_ok());
        assert!(timeout(Duration::from_millis(30), kik_peer.read_u8())
            .await
            .is_err());
    }

    #[test]
    fn catalog_json_contract_rejects_unknown_fields_and_keeps_only_names() {
        let json = r#"{"id":"request","target":"allowed","query":"","cursor":null,"limit":32,"binary":"secret.exe"}"#;
        assert!(serde_json::from_str::<TaskListRequest>(json).is_err());
        let page = TaskListPage {
            tasks: vec!["task_a".into()],
            next_cursor: Some("task_a".into()),
        };
        assert_eq!(
            serde_json::to_string(&page).unwrap(),
            r#"{"tasks":["task_a"],"next_cursor":"task_a"}"#
        );
        assert!(serde_json::from_str::<TaskListPage>(
            r#"{"tasks":[],"next_cursor":null,"path":"secret"}"#
        )
        .is_err());
    }
}
