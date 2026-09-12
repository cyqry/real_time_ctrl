pub mod backend;
mod desktop_duplication;
mod dxgi_backend;
mod gdi_backend;
mod legacy_gdi;

use common::hidden;

#[cfg(test)]
mod tests;

pub use self::backend::{CaptureRequest, ScreenCaptureService};
pub use self::desktop_duplication::{CaptureError, PngProfile, PrimaryScreenCapturer};
use self::dxgi_backend::DxgiDesktopDuplicationBackend;
use self::gdi_backend::LegacyGdiCaptureBackend;
pub use self::legacy_gdi::cut_screen;

static DEFAULT_CAPTURE_SERVICE: std::sync::OnceLock<ScreenCaptureService> =
    std::sync::OnceLock::new();

fn default_capture_service() -> &'static ScreenCaptureService {
    DEFAULT_CAPTURE_SERVICE.get_or_init(|| {
        ScreenCaptureService::builder()
            .backend(DxgiDesktopDuplicationBackend)
            .backend(LegacyGdiCaptureBackend)
            .build()
            .expect(&hidden!("默认截屏服务必须至少注册一个后端"))
    })
}

/// 使用默认后端管线截屏。新增后端只需实现接口并调整组合根。
pub async fn capture_screen(request: CaptureRequest) -> anyhow::Result<Vec<u8>> {
    default_capture_service().capture(request).await
}

/// 兼容既有调用名称，实际通过默认管线优先执行 DXGI，必要时再执行后续后端。
pub async fn cut_screen_dxgi(profile: PngProfile) -> anyhow::Result<Vec<u8>> {
    capture_screen(CaptureRequest::png(profile)).await
}
