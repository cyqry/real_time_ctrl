//! 一次 $run 的服务端生命周期：固定目标、加载配置、协商缓存或发送程序、返回启动/退出响应。
//!
//! 文件传输使用服务端生成的 ID，不借用 CtrlData 路由。传输完成即释放入站请求许可，
//! 等待同步程序退出不占 Kik 普通命令许可；不保存任务状态或自动重放执行。

use crate::{core::context::Context, tasks};
use common::{
    channel::Channel,
    file_util,
    message::{
        dok::Dok,
        kik_frame::{encode_data_frame, KikFrame},
        kik_resp::{ClientSuccessResp, KikResp},
    },
    protocol::{self},
    task::{self, TaskFrame, TASK_DATA_BINDING, TASK_MAIN_KEY, TASK_NAMED_CACHE_CAPABLE},
};
use ctrl_common::{ctrl_frame::Frame, ctrl_protocol::ctrl_kik_resp, kik::Kik};
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::{oneshot, Mutex, MutexGuard, OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
    time::{timeout, timeout_at, Instant},
};
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;

type Permits = (
    OwnedSemaphorePermit,
    OwnedSemaphorePermit,
    OwnedSemaphorePermit,
);

/// 只限制同时读取/发送二进制的流水线，避免 256 个入站请求各持有 12 MiB 分片。
/// 程序启动前即释放，与 Kik 上已经运行的同步或异步进程数量无关。
static TRANSFER_LIMIT: Semaphore = Semaphore::const_new(4);

/// 一次传输固定的授权与连接代次。分片任务只克隆共享句柄，不重新读取当前选中设备。
#[derive(Clone)]
struct TransferContext {
    context: Context,
    session: String,
    kik: Kik,
    main: Arc<Mutex<Channel>>,
    binding: [u8; 32],
}

impl TransferContext {
    fn new(
        context: &Context,
        session: &str,
        kik: &Kik,
        main: &Arc<Mutex<Channel>>,
        binding: [u8; 32],
    ) -> Self {
        Self {
            context: context.clone(),
            session: session.to_owned(),
            kik: kik.clone(),
            main: main.clone(),
            binding,
        }
    }
}

/// 主连接也可能正在发送其他响应；锁等待与单帧写超时分别有界，不能无限消耗任务预算。
async fn control_channel(channel: &Arc<Mutex<Channel>>) -> anyhow::Result<MutexGuard<'_, Channel>> {
    timeout(Duration::from_secs(5), channel.lock())
        .await
        .map_err(|_| anyhow::anyhow!("等待任务控制连接超时"))
}

/// 请求 future 被取消时也要移除等待者；这里只清理服务端内存，不终止已启动的远程进程。
/// 正常路径使用 finish 立即清理，Drop 是任务中止/提前返回时的兜底。
struct ResponseRegistration {
    kik: Kik,
    ids: Option<[String; 2]>,
}

impl ResponseRegistration {
    async fn finish(&mut self) {
        if let Some(ids) = &self.ids {
            for id in ids {
                self.kik.cancel_command(id).await;
            }
            self.ids = None;
        }
    }
}

impl Drop for ResponseRegistration {
    fn drop(&mut self) {
        let Some(ids) = self.ids.take() else { return };
        let kik = self.kik.clone();
        // 运行时关闭时所有连接和等待表都会一起销毁；此时无需再启动清理任务。
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                for id in ids {
                    kik.cancel_command(&id).await;
                }
            });
        }
    }
}

/// 控制会话结束只停止等待响应，Kik 仍独立承担本地同步超时或异步程序生命周期。
/// 不持有 Channel 锁，避免一个慢网络写入阻止会话失效检查。
async fn wait_session_end(context: &Context, session: &str, target: &str) {
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        if context
            .get_authorized_target(session, target)
            .await
            .is_err()
        {
            return;
        }
    }
}

/// 任务返回只能是文本或业务错误，不能让错误类型的成功帧触发控制端的数据下载分支。
fn validate_response(response: KikResp) -> anyhow::Result<KikResp> {
    match response {
        KikResp::Success(ClientSuccessResp::Info(_)) | KikResp::Error(_, _) => Ok(response),
        _ => anyhow::bail!("Kik 返回了不匹配的任务响应类型"),
    }
}

/// 准备回执复用内部 oneshot 表，但与执行结果使用不同 ID；此标记永不发送给控制端。
pub(super) fn preparation_receipt(cache_hit: bool) -> KikResp {
    KikResp::Success(ClientSuccessResp::Info(
        if cache_hit { "cache-hit" } else { "" }.to_owned(),
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PreparationReceipt {
    Transfer,
    CacheHit,
}

/// 拒绝准备可以直接返回业务错误；成功结果必须等回执，才能证明是否真正命中并禁止误传。
struct PreparationOutcome {
    receipt: Option<PreparationReceipt>,
    early_response: Option<KikResp>,
}

async fn wait_preparation(
    mut ready_rx: oneshot::Receiver<KikResp>,
    response_rx: &mut oneshot::Receiver<KikResp>,
    cache_capable: bool,
) -> anyhow::Result<PreparationOutcome> {
    let mut early_response = None;
    loop {
        tokio::select! {
            // 同一轮已有两个消息时先取回执；结果先到时保存结果，绝不能再次 poll 已消费的 oneshot。
            biased;
            receipt = &mut ready_rx => {
                let receipt = match receipt.map_err(|_| anyhow::anyhow!("Kik 已断开，准备结果未确认"))? {
                    KikResp::Success(ClientSuccessResp::Info(value)) if value.is_empty() => PreparationReceipt::Transfer,
                    KikResp::Success(ClientSuccessResp::Info(value)) if value == "cache-hit" && cache_capable => PreparationReceipt::CacheHit,
                    _ => anyhow::bail!("Kik 返回了不匹配的任务准备回执"),
                };
                return Ok(PreparationOutcome { receipt: Some(receipt), early_response });
            }
            response = &mut *response_rx, if early_response.is_none() => {
                let response = response.map_err(|_| anyhow::anyhow!("Kik 已断开，执行结果未确认"))?;
                if matches!(&response, KikResp::Success(ClientSuccessResp::Info(_))) {
                    early_response = Some(response);
                } else {
                    return Ok(PreparationOutcome { receipt: None, early_response: Some(response) });
                }
            }
        }
    }
}

/// 只有 Ready 才能读源流和写数据连接。命中或远端提前拒绝时直接丢弃源句柄，零分片发送。
async fn transfer_if_needed(
    transfer: &TransferContext,
    id: &str,
    stream: file_util::FileChunkStream,
    preparation: PreparationOutcome,
    response_rx: &mut oneshot::Receiver<KikResp>,
) -> anyhow::Result<Option<KikResp>> {
    if preparation.early_response.is_some() {
        return Ok(preparation.early_response);
    }
    match preparation.receipt {
        Some(PreparationReceipt::Transfer) => send_parts(transfer, id, stream, response_rx).await,
        Some(PreparationReceipt::CacheHit) => Ok(None),
        None => anyhow::bail!("任务准备缺少回执和响应"),
    }
}

fn prepare_request(id: &str, size: u64, hash: [u8; 32], config: &tasks::LoadedTask) -> TaskFrame {
    TaskFrame::PrepareNamed {
        id: id.to_owned(),
        size,
        hash,
        spec: config.spec.clone(),
        file_name: config.file_name.clone(),
    }
}

/// 列表与执行使用同一升级门槛；旧 Hello/HelloCached 只保留普通控制，不改变用户文件名意图。
pub(super) fn named_task_key(connection: &Channel) -> anyhow::Result<[u8; 32]> {
    if connection.attribute(&TASK_NAMED_CACHE_CAPABLE) != Some(&true) {
        anyhow::bail!("目标 Kik 不支持指定文件名任务，请升级 Kik");
    }
    connection
        .attribute(&TASK_MAIN_KEY)
        .copied()
        .ok_or_else(|| anyhow::anyhow!("目标 Kik 未完成任务能力协商，请升级 Kik"))
}

pub async fn execute(
    context: &Context,
    channel: &Arc<Mutex<Channel>>,
    session: &str,
    external_id: &str,
    name: &str,
    target: Option<&str>,
    permits: Permits,
) -> anyhow::Result<()> {
    let target = target.ok_or_else(|| anyhow::anyhow!("任务命令必须携带目标 Kik ID"))?;
    let kik = context.get_authorized_target(session, target).await?;
    let main = kik
        .get_kik_conn()
        .await
        .ok_or_else(|| anyhow::anyhow!("目标 Kik 已下线"))?;
    let target_id = kik.id().ok_or_else(|| anyhow::anyhow!("Kik 缺少 ID"))?;
    let key = {
        let main = control_channel(&main).await?;
        named_task_key(&main)?
    };
    let config = timeout(
        Duration::from_secs(task::PREPARE_SECONDS),
        tasks::load(&tasks::root()?, name),
    )
    .await
    .map_err(|_| anyhow::anyhow!("读取任务配置超时"))??;
    control_channel(channel)
        .await?
        .write_and_flush(&protocol::transfer_encode_frame(Frame::TaskBudget(
            external_id.to_owned(),
            config.spec.wait_seconds(),
        )))
        .await?;
    let transfer_permit = tokio::select! {
        permit = timeout(Duration::from_secs(task::PREPARE_SECONDS), TRANSFER_LIMIT.acquire()) => {
            permit.map_err(|_| anyhow::anyhow!("任务传输繁忙，本次任务未派发"))??
        }
        _ = wait_session_end(context, session, target_id) => anyhow::bail!("控制会话已结束，本次任务未派发"),
    };
    // prepare_big_file 在同一个打开句柄上算摘要并创建流；不会根据路径重新打开另一个版本。
    // 命中仍须每次检查当前源内容，只省略后续流读取和网络分片，不能缓存旧配置或旧 hash。
    let prepared = tokio::select! {
        prepared = timeout(Duration::from_secs(task::PREPARE_SECONDS),
            file_util::prepare_big_file(&config.binary, file_util::FILE_TRANSFER_CHUNK_BYTES)) => {
            prepared.map_err(|_| anyhow::anyhow!("读取任务程序超时"))?
                .map_err(|_| anyhow::anyhow!("读取任务程序失败"))?
        }
        _ = wait_session_end(context, session, target_id) => anyhow::bail!("控制会话已结束，本次任务未派发"),
    };
    if prepared.size == 0 {
        anyhow::bail!("任务程序为空");
    }
    let id = uuid::Uuid::new_v4().to_string();
    let account = context.account_id_for_audit(session).await?;
    let ready_id = format!("{id}-ready");
    let ready_rx = kik.register_command(ready_id.clone()).await?;
    let mut response_rx = match kik.register_command(id.clone()).await {
        Ok(rx) => rx,
        Err(error) => {
            kik.cancel_command(&ready_id).await;
            return Err(error);
        }
    };
    let mut registration = ResponseRegistration {
        kik: kik.clone(),
        ids: Some([ready_id, id.clone()]),
    };
    let result = async {
        // 摘要准备可能耗时，派发前再次检查会话授权和主连接身份。
        context.get_authorized_target(session, target_id).await?;
        if !kik.is_kik_conn(&main).await || control_channel(channel).await?.is_closed() {
            anyhow::bail!("连接已变化，本次任务未派发");
        }
        let spec = config.spec.clone();
        let hash = prepared.hash.as_slice().try_into().map_err(|_| anyhow::anyhow!("任务摘要错误"))?;
        let request = prepare_request(&id, prepared.size, hash, &config);
        log::info!("任务分派: account={}, kik={}, task={}, request={}, mode={}",
            account, target_id, name, id, if spec.asynchronous { "async" } else { "sync" });
        control_channel(&main).await?.write_and_flush(&protocol::transfer_encode_frame(KikFrame::Task(request))).await?;
        let preparation = tokio::select! {
            prepared = timeout(Duration::from_secs(task::PREPARE_SECONDS + task::FINISH_SECONDS), wait_preparation(ready_rx, &mut response_rx, true)) => {
                prepared.map_err(|_| anyhow::anyhow!("Kik 校验缓存或准备接收超时，执行结果未确认"))??
            }
            _ = wait_session_end(context, session, target_id) => anyhow::bail!("控制会话已结束，执行结果未确认"),
        };
        match preparation.receipt {
            Some(PreparationReceipt::CacheHit) => log::info!("任务缓存: account={}, kik={}, task={}, request={}, cache=hit, transferred_bytes=0", account, target_id, name, id),
            Some(PreparationReceipt::Transfer) => log::info!("任务缓存: account={}, kik={}, task={}, request={}, cache=miss, expected_bytes={}", account, target_id, name, id, prepared.size),
            None => {}
        }
        let transfer_context = TransferContext::new(context, session, &kik, &main, task::binding_id(&key));
        let transfer = transfer_if_needed(&transfer_context, &id, prepared.stream, preparation, &mut response_rx).await;
        // 传输背压与任务执行等待分离：后续运行多久都不占用传输槽位。
        drop(transfer_permit);
        drop(permits);
        // 最后一个分片收到后，快速程序可能在服务器写任务返回前就完成了，不能再读同一个 oneshot。
        if let Some(response) = transfer? { return Ok(response); }
        tokio::select! {
            response = timeout(Duration::from_secs(u64::from(spec.timeout_seconds) + task::PREPARE_SECONDS + task::FINISH_SECONDS), response_rx) => {
                response.map_err(|_| anyhow::anyhow!("等待任务响应超时，执行结果未确认"))?
                    .map_err(|_| anyhow::anyhow!("Kik 已断开，执行结果未确认"))
            }
            _ = wait_session_end(context, session, target_id) => Err(anyhow::anyhow!("控制会话已结束，执行结果未确认")),
        }
    }.await;
    let response_class = match &result {
        Ok(KikResp::Success(ClientSuccessResp::Info(_))) => "success",
        Ok(KikResp::Error(_, _)) => "remote_error",
        Ok(_) => "invalid_response",
        Err(_) => "unconfirmed",
    };
    log::info!(
        "任务响应: account={}, kik={}, task={}, request={}, result={}",
        account,
        target_id,
        name,
        id,
        response_class
    );
    if (result.is_err() || matches!(&result, Ok(KikResp::Error(_, _))))
        && kik.is_kik_conn(&main).await
    {
        // send_parts 返回前会等完已发起写入。此消息只释放尚未启动的接收，不能杀死程序。
        // 连接锁等待有界；实际写入由 Channel 自身超时负责，不能外部取消半帧后复用连接。
        if let Ok(mut connection) = timeout(Duration::from_secs(5), main.lock()).await {
            if !connection.is_closed() {
                let _ = connection
                    .write_and_flush(&protocol::transfer_encode_frame(KikFrame::Task(
                        TaskFrame::AbandonTransfer(id.clone()),
                    )))
                    .await;
            }
        }
    }
    registration.finish().await;
    let response = validate_response(result?)?;
    control_channel(channel)
        .await?
        .write_and_flush(&ctrl_kik_resp(external_id.to_owned(), response))
        .await
}

/// 最多保留三个分片；任何失败后排空已经开始的写入，不能把半帧留在复用连接中。
async fn send_parts(
    transfer: &TransferContext,
    id: &str,
    mut stream: file_util::FileChunkStream,
    response_rx: &mut oneshot::Receiver<KikResp>,
) -> anyhow::Result<Option<KikResp>> {
    let (context, session, kik) = (&transfer.context, transfer.session.as_str(), &transfer.kik);
    let target = kik.id().ok_or_else(|| anyhow::anyhow!("Kik 缺少 ID"))?;
    let deadline = Instant::now() + Duration::from_secs(task::TRANSFER_SECONDS);
    let mut pending = JoinSet::new();
    let mut exhausted = false;
    let mut failure = None;
    let mut early_response = None;
    let mut response_open = true;
    let stop_writes = CancellationToken::new();
    loop {
        while !exhausted
            && failure.is_none()
            && pending.len() < file_util::FILE_TRANSFER_IN_FLIGHT_PARTS
        {
            if context
                .get_authorized_target(session, target)
                .await
                .is_err()
            {
                failure = Some(anyhow::anyhow!("控制会话已结束，停止任务传输"));
                break;
            }
            let next = tokio::select! {
                // 已到达的磁盘/接收错误优先于继续读源文件，不把整个 EXE 发送给已拒绝的接收端。
                biased;
                response = &mut *response_rx, if response_open => {
                    response_open = false;
                    record_early_response(response, &mut early_response, &mut failure, &stop_writes);
                    exhausted = true;
                    continue;
                }
                next = timeout_at(deadline, stream.next()) => next,
            };
            match next {
                Ok(Some(Ok((range, bytes)))) => {
                    // 编码错误也必须走 drain，不能用 ? 提前丢弃 JoinSet 并取消其他半帧写入。
                    let frame = range
                        .end
                        .checked_sub(1)
                        .ok_or_else(|| anyhow::anyhow!("任务分片区间为空"))
                        .and_then(|end| {
                            Dok::encode_file_part(range.start, end, &bytes).map_err(Into::into)
                        })
                        .and_then(|part| encode_data_frame(id, &part).map_err(Into::into));
                    let frame = match frame {
                        Ok(frame) => frame,
                        Err(error) => {
                            failure = Some(error);
                            break;
                        }
                    };
                    let transfer = transfer.clone();
                    let stop = stop_writes.clone();
                    pending
                        .spawn(async move { send_frame(&transfer, &frame, deadline, stop).await });
                }
                Ok(None) => exhausted = true,
                _ => failure = Some(anyhow::anyhow!("读取任务分片失败或超时")),
            }
        }
        if pending.is_empty() {
            break;
        }
        if failure.is_some() {
            stop_writes.cancel();
        }
        let completed = tokio::select! {
            biased;
            response = &mut *response_rx, if response_open => {
                response_open = false;
                record_early_response(response, &mut early_response, &mut failure, &stop_writes);
                exhausted = true;
                continue;
            }
            completed = pending.join_next() => completed,
        };
        match completed {
            Some(Ok(Ok(()))) => {}
            _ if failure.is_none() => {
                failure = Some(anyhow::anyhow!("发送任务程序失败，执行结果未确认"))
            }
            _ => {}
        }
    }
    // 分片写入与主连接响应来自独立异步任务，最后一次 join 和本次读取之间可能已有响应到达。
    if response_open {
        match response_rx.try_recv() {
            Ok(response) => early_response = Some(response),
            Err(oneshot::error::TryRecvError::Closed) if failure.is_none() => {
                failure = Some(anyhow::anyhow!("Kik 已断开，执行结果未确认"));
            }
            _ => {}
        }
    }
    // 接收端的真实结果比伴随其拒绝而产生的传输写错误更有信息，保持原错误码和文案。
    if let Some(response) = early_response {
        return Ok(Some(response));
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(None),
    }
}

fn record_early_response(
    response: Result<KikResp, oneshot::error::RecvError>,
    result: &mut Option<KikResp>,
    failure: &mut Option<anyhow::Error>,
    stop_writes: &CancellationToken,
) {
    match response {
        Ok(response) => {
            if !matches!(&response, KikResp::Success(ClientSuccessResp::Info(_))) {
                stop_writes.cancel();
            }
            *result = Some(response);
        }
        Err(_) => {
            *failure = Some(anyhow::anyhow!("Kik 已断开，执行结果未确认"));
            stop_writes.cancel();
        }
    }
}

async fn send_frame(
    transfer: &TransferContext,
    bytes: &[u8],
    deadline: Instant,
    stop_writes: CancellationToken,
) -> anyhow::Result<()> {
    let (context, session, kik, main, binding) = (
        &transfer.context,
        transfer.session.as_str(),
        &transfer.kik,
        &transfer.main,
        transfer.binding,
    );
    let target = kik.id().ok_or_else(|| anyhow::anyhow!("Kik 缺少 ID"))?;
    let recovery = deadline.min(Instant::now() + common::channel::DATA_CONNECTION_RECOVERY_TIMEOUT);
    loop {
        if stop_writes.is_cancelled()
            || Instant::now() >= recovery
            || !kik.is_kik_conn(main).await
            || context
                .get_authorized_target(session, target)
                .await
                .is_err()
        {
            anyhow::bail!("任务数据连接不可用或主连接已变化");
        }
        for connection in kik.data_connections_for_send().await {
            // 大文件转发也会占用同一连接的写锁；等待锁同样计入传输截止时间。
            let mut conn = tokio::select! {
                connection = timeout_at(deadline, connection.lock()) => {
                    connection.map_err(|_| anyhow::anyhow!("等待任务数据连接超时"))?
                }
                _ = wait_session_end(context, session, target) => anyhow::bail!("控制会话已结束，停止任务传输"),
                _ = stop_writes.cancelled() => anyhow::bail!("Kik 已返回结果，停止任务传输"),
            };
            if conn.is_closed() || conn.attribute(&TASK_DATA_BINDING) != Some(&binding) {
                continue;
            }
            let written = tokio::select! {
                written = timeout_at(deadline, conn.write_and_flush(bytes)) => matches!(written, Ok(Ok(()))),
                _ = wait_session_end(context, session, target) => false,
                _ = stop_writes.cancelled() => false,
            };
            if written {
                return Ok(());
            } else {
                // shutdown 会先标记 closed；即使对端不响应关闭，后续发送也不会复用半帧。
                // 不能让数据通道默认六分钟的 shutdown 时限再次延长整项传输截止时间。
                let _ = timeout(Duration::from_secs(5), conn.try_write_half_close()).await;
            }
        }
        // 仅在连接池重建时短暂等待，不保留无界重放队列。
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::channel::ChannelType;
    use common::protocol::BufSerializable;

    fn kik() -> Kik {
        let (_peer, stream) = tokio::io::duplex(64);
        let channel = Arc::new(Mutex::new(Channel::new(
            Box::pin(stream),
            Some("test-kik".into()),
            ChannelType::Kik,
            Err(std::io::Error::other("test")),
            Err(std::io::Error::other("test")),
        )));
        Kik::new(
            "test-kik",
            "test",
            "127.0.0.1".into(),
            std::time::SystemTime::now(),
            channel,
        )
    }

    async fn authorized_kik() -> (Context, Kik, Arc<Mutex<Channel>>) {
        let context = Context::init();
        let kik = kik();
        kik.set_kik_initialized(true);
        context
            .kiks
            .write()
            .await
            .insert("test-kik".into(), kik.clone());
        let (_peer, stream) = tokio::io::duplex(64);
        let control = Arc::new(Mutex::new(Channel::new(
            Box::pin(stream),
            Some("controller".into()),
            ChannelType::Ctrl,
            Err(std::io::Error::other("test")),
            Err(std::io::Error::other("test")),
        )));
        context
            .register_ctrl_session(
                control,
                "session".into(),
                "default".into(),
                "instance".into(),
            )
            .await
            .unwrap();
        let main = kik.get_kik_conn().await.unwrap();
        (context, kik, main)
    }

    #[tokio::test]
    async fn early_response_is_preserved_without_reading_or_repolling_oneshot() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for response in [
            KikResp::Error(77, "temporary disk is full".into()),
            KikResp::Success(ClientSuccessResp::Info("started".into())),
        ] {
            let (context, kik, main) = authorized_kik().await;
            let (sender, mut receiver) = oneshot::channel();
            sender.send(response).unwrap();
            let polls = Arc::new(AtomicUsize::new(0));
            let observed = polls.clone();
            let source = Box::pin(futures::stream::poll_fn(move |_| {
                observed.fetch_add(1, Ordering::Relaxed);
                std::task::Poll::Pending
            }));
            let response = timeout(
                Duration::from_secs(3),
                send_parts(
                    &TransferContext::new(&context, "session", &kik, &main, [0; 32]),
                    "run",
                    source,
                    &mut receiver,
                ),
            )
            .await
            .unwrap()
            .unwrap()
            .unwrap();
            assert!(matches!(
                response,
                KikResp::Error(77, _) | KikResp::Success(ClientSuccessResp::Info(_))
            ));
            assert_eq!(polls.load(Ordering::Relaxed), 0);
        }
    }

    #[tokio::test]
    async fn cache_hit_drops_the_source_without_polling_or_writing_any_data() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::AsyncReadExt;
        let (context, kik, main) = authorized_kik().await;
        let (mut peer, stream) = tokio::io::duplex(4096);
        let data = Arc::new(Mutex::new(Channel::new(
            Box::pin(stream),
            Some("data".into()),
            ChannelType::KikData,
            Err(std::io::Error::other("test")),
            Err(std::io::Error::other("test")),
        )));
        data.lock()
            .await
            .insert_attribute(&TASK_DATA_BINDING, [0; 32]);
        assert!(kik.insert_data_conn(data).await);
        let (_sender, mut receiver) = oneshot::channel();
        let polls = Arc::new(AtomicUsize::new(0));
        let observed = polls.clone();
        let source = Box::pin(futures::stream::poll_fn(move |_| {
            observed.fetch_add(1, Ordering::Relaxed);
            std::task::Poll::Ready(Some(Ok((0..1, vec![7]))))
        }));
        let result = transfer_if_needed(
            &TransferContext::new(&context, "session", &kik, &main, [0; 32]),
            "run",
            source,
            PreparationOutcome {
                receipt: Some(PreparationReceipt::CacheHit),
                early_response: None,
            },
            &mut receiver,
        )
        .await
        .unwrap();
        assert!(result.is_none());
        assert_eq!(polls.load(Ordering::Relaxed), 0);
        assert!(timeout(Duration::from_millis(30), peer.read_u8())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn final_success_before_cache_receipt_is_preserved_and_waits_for_receipt() {
        let (receipt_tx, receipt_rx) = oneshot::channel();
        let (response_tx, mut response_rx) = oneshot::channel();
        response_tx
            .send(KikResp::Success(ClientSuccessResp::Info("done".into())))
            .unwrap();
        let waiter = wait_preparation(receipt_rx, &mut response_rx, true);
        tokio::pin!(waiter);
        assert!(timeout(Duration::from_millis(20), &mut waiter)
            .await
            .is_err());
        receipt_tx.send(preparation_receipt(true)).unwrap();
        let outcome = timeout(Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.receipt, Some(PreparationReceipt::CacheHit));
        assert!(
            matches!(outcome.early_response, Some(KikResp::Success(ClientSuccessResp::Info(message))) if message == "done")
        );
    }

    #[tokio::test]
    async fn preparation_rejection_needs_no_receipt_and_unnegotiated_hit_is_rejected() {
        let (_receipt_tx, receipt_rx) = oneshot::channel();
        let (response_tx, mut response_rx) = oneshot::channel();
        response_tx
            .send(KikResp::Error(9, "cache is locked".into()))
            .unwrap();
        let outcome = wait_preparation(receipt_rx, &mut response_rx, true)
            .await
            .unwrap();
        assert!(outcome.receipt.is_none());
        assert!(matches!(outcome.early_response, Some(KikResp::Error(9, _))));

        for capable in [false, true] {
            let (receipt_tx, receipt_rx) = oneshot::channel();
            let (_response_tx, mut response_rx) = oneshot::channel();
            receipt_tx.send(preparation_receipt(true)).unwrap();
            let result = wait_preparation(receipt_rx, &mut response_rx, capable).await;
            assert_eq!(result.is_ok(), capable);
        }
        let (receipt_tx, receipt_rx) = oneshot::channel();
        let (_response_tx, mut response_rx) = oneshot::channel();
        receipt_tx.send(preparation_receipt(false)).unwrap();
        let outcome = wait_preparation(receipt_rx, &mut response_rx, false)
            .await
            .unwrap();
        assert_eq!(outcome.receipt, Some(PreparationReceipt::Transfer));
    }

    #[tokio::test]
    async fn ready_still_sends_file_parts_on_a_bound_data_connection() {
        use common::protocol::BufSerializable;
        use tokio::io::AsyncReadExt;
        let (context, kik, main) = authorized_kik().await;
        let (mut peer, stream) = tokio::io::duplex(4096);
        let data = Arc::new(Mutex::new(Channel::new(
            Box::pin(stream),
            Some("data".into()),
            ChannelType::KikData,
            Err(std::io::Error::other("test")),
            Err(std::io::Error::other("test")),
        )));
        data.lock()
            .await
            .insert_attribute(&TASK_DATA_BINDING, [0; 32]);
        assert!(kik.insert_data_conn(data).await);
        let (_sender, mut receiver) = oneshot::channel();
        let source = Box::pin(tokio_stream::iter(vec![Ok((0..3, b"exe".to_vec()))]));
        let result = transfer_if_needed(
            &TransferContext::new(&context, "session", &kik, &main, [0; 32]),
            "run",
            source,
            PreparationOutcome {
                receipt: Some(PreparationReceipt::Transfer),
                early_response: None,
            },
            &mut receiver,
        )
        .await
        .unwrap();
        assert!(result.is_none());
        let length = timeout(Duration::from_secs(1), peer.read_u32())
            .await
            .unwrap()
            .unwrap();
        let mut bytes = vec![0; length as usize];
        peer.read_exact(&mut bytes).await.unwrap();
        assert!(
            matches!(KikFrame::from_buf(bytes.as_slice().into()), Some(KikFrame::Data(id, _)) if id == "run")
        );
    }

    #[test]
    fn task_preparation_uses_only_the_configured_file_name_without_source_path() {
        let config = tasks::LoadedTask {
            binary: "never-read-original.exe".into(),
            spec: task::TaskSpec {
                asynchronous: false,
                output: true,
                timeout_seconds: 2,
                args: vec![],
                default_content: String::new(),
            },
            file_name: "中文 程序.EXE".into(),
        };
        assert!(
            matches!(prepare_request("run", 3, [8; 32], &config), TaskFrame::PrepareNamed { file_name, .. } if file_name == config.file_name)
        );
        let bytes = prepare_request("run", 3, [8; 32], &config).to_buf();
        assert!(!bytes
            .windows(b"never-read-original.exe".len())
            .any(|window| window == b"never-read-original.exe"));
    }

    #[tokio::test]
    async fn old_kik_task_request_fails_before_loading_or_sending_any_budget() {
        use common::task::TASK_CACHE_CAPABLE;
        use tokio::io::AsyncReadExt;
        for cached in [false, true] {
            let (context, _kik, main) = authorized_kik().await;
            {
                let mut main = main.lock().await;
                main.insert_attribute(&TASK_MAIN_KEY, [7; 32]);
                main.insert_attribute(&TASK_CACHE_CAPABLE, cached);
            }
            let (mut peer, writer) = tokio::io::duplex(256);
            let controller = Arc::new(Mutex::new(Channel::new(
                Box::pin(writer),
                None,
                ChannelType::Ctrl,
                Err(std::io::Error::other("test")),
                Err(std::io::Error::other("test")),
            )));
            let error = execute(
                &context,
                &controller,
                "session",
                "external",
                "does_not_exist",
                Some("test-kik"),
                context.try_acquire_command("session").await.unwrap(),
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains("升级 Kik"));
            assert!(timeout(Duration::from_millis(20), peer.read_u8())
                .await
                .is_err());
            assert!(context.try_acquire_command("session").await.is_ok());
        }
    }

    #[tokio::test]
    async fn receiver_error_stops_blocked_fragment_and_preserves_the_real_error() {
        use tokio::io::AsyncReadExt;
        let (context, kik, main) = authorized_kik().await;
        let (mut peer, stream) = tokio::io::duplex(64);
        let data = Arc::new(Mutex::new(Channel::new(
            Box::pin(stream),
            Some("data".into()),
            ChannelType::KikData,
            Err(std::io::Error::other("test")),
            Err(std::io::Error::other("test")),
        )));
        data.lock()
            .await
            .insert_attribute(&TASK_DATA_BINDING, [0; 32]);
        assert!(kik.insert_data_conn(data.clone()).await);
        let (sender, mut receiver) = oneshot::channel();
        let remote = tokio::spawn(async move {
            // 确认写入已经开始，再让远端返回磁盘错误；其余字节保持不读以模拟网络背压。
            let _ = peer.read_u8().await.unwrap();
            sender
                .send(KikResp::Error(
                    78,
                    "cannot preallocate temporary program".into(),
                ))
                .unwrap();
            std::future::pending::<()>().await;
        });
        let source = Box::pin(
            tokio_stream::iter(vec![Ok((0..4096, vec![1; 4096]))]).chain(tokio_stream::pending()),
        );
        let response = timeout(
            Duration::from_secs(3),
            send_parts(
                &TransferContext::new(&context, "session", &kik, &main, [0; 32]),
                "run",
                source,
                &mut receiver,
            ),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert!(
            matches!(response, KikResp::Error(78, message) if message == "cannot preallocate temporary program")
        );
        assert!(data.lock().await.is_closed());
        remote.abort();
        let _ = remote.await;
    }

    #[tokio::test]
    async fn dropped_registration_cleans_only_its_own_waiters() {
        let kik = kik();
        let first = kik.register_command("first".into()).await.unwrap();
        let second = kik.register_command("second".into()).await.unwrap();
        let unrelated = kik.register_command("unrelated".into()).await.unwrap();
        drop(ResponseRegistration {
            kik: kik.clone(),
            ids: Some(["first".into(), "second".into()]),
        });
        assert!(timeout(Duration::from_secs(1), first)
            .await
            .unwrap()
            .is_err());
        assert!(timeout(Duration::from_secs(1), second)
            .await
            .unwrap()
            .is_err());
        assert!(
            kik.complete_command("unrelated", KikResp::Error(1, "expected".into()))
                .await
        );
        assert!(unrelated.await.is_ok());
    }

    #[test]
    fn task_reply_cannot_open_an_unrelated_data_route() {
        assert!(validate_response(KikResp::Success(ClientSuccessResp::Info("ok".into()))).is_ok());
        assert!(validate_response(KikResp::Error(1, "business failure".into())).is_ok());
        assert!(
            validate_response(KikResp::Success(ClientSuccessResp::DataId(
                "foreign".into()
            )))
            .is_err()
        );
    }

    #[tokio::test]
    async fn missing_control_session_ends_wait_without_a_remote_cancel() {
        assert!(timeout(
            Duration::from_secs(1),
            wait_session_end(&Context::init(), "missing", "target")
        )
        .await
        .is_ok());
    }
}
