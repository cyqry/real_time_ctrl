use super::backend::{CaptureArtifact, CaptureFuture, CaptureRequest, ScreenCaptureBackend};
use super::{CaptureError, PngProfile, PrimaryScreenCapturer};
use anyhow::anyhow;
use common::hidden;
use std::time::Duration;

struct DxgiCaptureRequest {
    profile: PngProfile,
    reply: tokio::sync::oneshot::Sender<anyhow::Result<(Vec<u8>, bool)>>,
}

enum WorkerEvent<T> {
    Request(T),
    SessionExpired,
    Closed,
}

static DXGI_CAPTURE_WORKER: std::sync::OnceLock<
    Result<tokio::sync::mpsc::Sender<DxgiCaptureRequest>, String>,
> = std::sync::OnceLock::new();
const DXGI_QUEUE_TIMEOUT: Duration = Duration::from_secs(5);
const DXGI_RESULT_TIMEOUT: Duration = Duration::from_secs(30);
const DXGI_SESSION_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const DXGI_STEADY_ACQUIRE_TIMEOUT_MS: u32 = 100;

#[derive(Debug, Default)]
pub struct DxgiDesktopDuplicationBackend;

impl ScreenCaptureBackend for DxgiDesktopDuplicationBackend {
    fn name(&self) -> String {
        hidden!("dxgi-desktop-duplication")
    }

    fn capture(&self, request: CaptureRequest) -> CaptureFuture<'_> {
        Box::pin(async move {
            let (png, is_uniform_black) = capture_dxgi(request.png_profile()).await?;
            if is_uniform_black {
                Ok(CaptureArtifact::png_needing_fallback(png))
            } else {
                Ok(CaptureArtifact::accepted_png(png))
            }
        })
    }
}

async fn capture_dxgi(profile: PngProfile) -> anyhow::Result<(Vec<u8>, bool)> {
    let worker = dxgi_capture_worker()?;
    let (reply, response) = tokio::sync::oneshot::channel();
    tokio::time::timeout(
        DXGI_QUEUE_TIMEOUT,
        worker.send(DxgiCaptureRequest { profile, reply }),
    )
    .await
    .map_err(|_| anyhow!(hidden!("DXGI 截屏请求排队超时")))?
    .map_err(|_| anyhow!(hidden!("DXGI 截屏工作线程已停止")))?;

    tokio::time::timeout(DXGI_RESULT_TIMEOUT, response)
        .await
        .map_err(|_| anyhow!(hidden!("DXGI 截屏处理超时")))?
        .map_err(|_| anyhow!(hidden!("DXGI 截屏工作线程未返回结果")))?
}

fn dxgi_capture_worker() -> anyhow::Result<tokio::sync::mpsc::Sender<DxgiCaptureRequest>> {
    match DXGI_CAPTURE_WORKER.get_or_init(|| {
        // 容量 1 限制并发截屏的排队内存，Desktop Duplication 本身也要求串行取帧。
        let (sender, receiver) = tokio::sync::mpsc::channel::<DxgiCaptureRequest>(1);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .map_err(|error| error.to_string())?;

        std::thread::Builder::new()
            .name(hidden!("ctrl-kik-dxgi-capture"))
            .spawn(move || {
                runtime.block_on(run_dxgi_worker(receiver, DXGI_SESSION_IDLE_TIMEOUT));
            })
            .map_err(|error| error.to_string())?;
        Ok(sender)
    }) {
        Ok(sender) => Ok(sender.clone()),
        Err(error) => Err(anyhow!(hidden!("无法启动 DXGI 截屏工作线程: ", error))),
    }
}

async fn run_dxgi_worker(
    mut receiver: tokio::sync::mpsc::Receiver<DxgiCaptureRequest>,
    session_idle_timeout: Duration,
) {
    let mut capturer = None;
    loop {
        match next_worker_event(&mut receiver, capturer.is_some(), session_idle_timeout).await {
            WorkerEvent::Request(request) => {
                let result = capture_dxgi_on_worker(&mut capturer, request.profile);
                let _ = request.reply.send(result);
            }
            WorkerEvent::SessionExpired => {
                // D3D/DXGI COM 对象必须在创建它们的工作线程内释放。
                capturer = None;
            }
            WorkerEvent::Closed => break,
        }
    }
}

async fn next_worker_event<T>(
    receiver: &mut tokio::sync::mpsc::Receiver<T>,
    session_active: bool,
    session_idle_timeout: Duration,
) -> WorkerEvent<T> {
    if !session_active {
        return match receiver.recv().await {
            Some(request) => WorkerEvent::Request(request),
            None => WorkerEvent::Closed,
        };
    }

    match tokio::time::timeout(session_idle_timeout, receiver.recv()).await {
        Ok(Some(request)) => WorkerEvent::Request(request),
        Ok(None) => WorkerEvent::Closed,
        Err(_) => WorkerEvent::SessionExpired,
    }
}

fn capture_dxgi_on_worker(
    capturer: &mut Option<PrimaryScreenCapturer>,
    profile: PngProfile,
) -> anyhow::Result<(Vec<u8>, bool)> {
    let mut last_error: Option<CaptureError> = None;
    for attempt in 0..2 {
        if capturer.is_none() {
            match PrimaryScreenCapturer::new() {
                Ok(created) => *capturer = Some(created),
                Err(error) if attempt == 0 && error.is_rebuildable() => {
                    last_error = Some(error);
                    continue;
                }
                Err(error) => return Err(error.into()),
            }
        }

        let active = capturer
            .as_mut()
            .ok_or_else(|| anyhow!(hidden!("DXGI 截屏会话未创建")))?;
        let mut png = Vec::new();
        match active.capture_png_into(profile, &mut png) {
            Ok(()) => {
                let is_uniform_black = active.last_frame_is_uniform_black();
                // 首次完整帧允许较长等待；已有缓存帧后缩短静止桌面的等待时间。
                active.set_acquire_timeout_ms(DXGI_STEADY_ACQUIRE_TIMEOUT_MS);
                return Ok((png, is_uniform_black));
            }
            Err(error) if attempt == 0 && error.is_rebuildable() => {
                // 只对设备丢失、重置等瞬时错误重建；其他错误交给管线决定是否降级。
                last_error = Some(error);
                *capturer = None;
            }
            Err(error) => return Err(error.into()),
        }
    }

    Err(last_error
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow!(hidden!("DXGI 截屏失败且没有错误详情"))))
}

#[cfg(test)]
mod tests {
    use super::{next_worker_event, Duration, WorkerEvent};

    #[tokio::test]
    async fn active_session_expires_after_idle_timeout() {
        let (_sender, mut receiver) = tokio::sync::mpsc::channel::<u8>(1);

        let event = next_worker_event(&mut receiver, true, Duration::from_millis(5)).await;

        assert!(matches!(event, WorkerEvent::SessionExpired));
    }

    #[tokio::test]
    async fn inactive_session_waits_for_next_request() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        sender.send(7_u8).await.expect("测试请求应可入队");

        let event = next_worker_event(&mut receiver, false, Duration::from_millis(0)).await;

        assert!(matches!(event, WorkerEvent::Request(7)));
    }
}
