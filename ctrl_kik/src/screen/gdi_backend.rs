use super::backend::{CaptureArtifact, CaptureFuture, CaptureRequest, ScreenCaptureBackend};

#[derive(Debug, Default)]
pub struct LegacyGdiCaptureBackend;

impl ScreenCaptureBackend for LegacyGdiCaptureBackend {
    fn name(&self) -> &'static str {
        "legacy-gdi"
    }

    fn capture(&self, _request: CaptureRequest) -> CaptureFuture<'_> {
        Box::pin(async move {
            let png = super::legacy_gdi::cut_screen().await?;
            // GDI 是最终兼容后端；纯黑可能是合法桌面，因此直接接受其结果。
            Ok(CaptureArtifact::accepted_png(png))
        })
    }
}
