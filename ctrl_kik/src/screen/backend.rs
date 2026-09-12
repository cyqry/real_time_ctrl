use super::PngProfile;
use anyhow::{anyhow, Result};
use common::hidden;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// 一次截屏请求。后续新增区域、屏幕编号等参数时，后端接口无需变化。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureRequest {
    png_profile: PngProfile,
}

impl CaptureRequest {
    pub const fn png(png_profile: PngProfile) -> Self {
        Self { png_profile }
    }

    pub const fn png_profile(self) -> PngProfile {
        self.png_profile
    }
}

/// 后端对当前产物的判断，由管线统一决定是否继续执行后续后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureDisposition {
    Accept,
    VerifyWithFallback,
}

/// 截屏后端的标准输出。
#[derive(Debug)]
pub struct CaptureArtifact {
    png: Vec<u8>,
    disposition: CaptureDisposition,
}

impl CaptureArtifact {
    pub fn accepted_png(png: Vec<u8>) -> Self {
        Self {
            png,
            disposition: CaptureDisposition::Accept,
        }
    }

    pub fn png_needing_fallback(png: Vec<u8>) -> Self {
        Self {
            png,
            disposition: CaptureDisposition::VerifyWithFallback,
        }
    }

    pub fn png(&self) -> &[u8] {
        &self.png
    }

    pub const fn disposition(&self) -> CaptureDisposition {
        self.disposition
    }

    fn into_png(self) -> Vec<u8> {
        self.png
    }
}

pub type CaptureFuture<'a> = Pin<Box<dyn Future<Output = Result<CaptureArtifact>> + Send + 'a>>;

/// 所有截屏实现只依赖该接口；后端不得直接调用另一个具体后端。
pub trait ScreenCaptureBackend: Send + Sync {
    fn name(&self) -> String;
    fn capture(&self, request: CaptureRequest) -> CaptureFuture<'_>;
}

/// 按注册顺序执行后端的稳定调度层。
pub struct ScreenCaptureService {
    backends: Vec<Arc<dyn ScreenCaptureBackend>>,
}

impl ScreenCaptureService {
    pub fn builder() -> ScreenCaptureServiceBuilder {
        ScreenCaptureServiceBuilder::default()
    }

    pub async fn capture(&self, request: CaptureRequest) -> Result<Vec<u8>> {
        let mut failures = Vec::with_capacity(self.backends.len());

        for backend in &self.backends {
            let name = backend.name();
            match backend.capture(request).await {
                Ok(artifact) if artifact.png().is_empty() => {
                    failures.push(hidden!(&name, ": 返回了空 PNG"));
                }
                Ok(artifact) if artifact.disposition() == CaptureDisposition::Accept => {
                    return Ok(artifact.into_png());
                }
                Ok(_) => {
                    failures.push(hidden!(&name, ": 当前帧需要后端复核"));
                }
                Err(error) => {
                    failures.push(hidden!(&name, ": ", error));
                }
            }
        }

        Err(anyhow!(hidden!(
            "所有截屏后端均未返回可接受结果: ",
            failures.join(&hidden!(" | "))
        )))
    }
}

#[derive(Default)]
pub struct ScreenCaptureServiceBuilder {
    backends: Vec<Arc<dyn ScreenCaptureBackend>>,
}

impl ScreenCaptureServiceBuilder {
    pub fn backend<B>(mut self, backend: B) -> Self
    where
        B: ScreenCaptureBackend + 'static,
    {
        self.backends.push(Arc::new(backend));
        self
    }

    pub fn build(self) -> Result<ScreenCaptureService> {
        if self.backends.is_empty() {
            return Err(anyhow!(hidden!("截屏服务至少需要一个后端")));
        }
        Ok(ScreenCaptureService {
            backends: self.backends,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Clone)]
    enum StubResult {
        Accept(Vec<u8>),
        Verify(Vec<u8>),
        Fail(&'static str),
    }

    struct StubBackend {
        name: &'static str,
        result: StubResult,
        calls: Arc<AtomicUsize>,
    }

    impl StubBackend {
        fn new(name: &'static str, result: StubResult, calls: Arc<AtomicUsize>) -> Self {
            Self {
                name,
                result,
                calls,
            }
        }
    }

    impl ScreenCaptureBackend for StubBackend {
        fn name(&self) -> String {
            self.name.to_string()
        }

        fn capture(&self, _request: CaptureRequest) -> CaptureFuture<'_> {
            let result = self.result.clone();
            let calls = self.calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::SeqCst);
                match result {
                    StubResult::Accept(png) => Ok(CaptureArtifact::accepted_png(png)),
                    StubResult::Verify(png) => Ok(CaptureArtifact::png_needing_fallback(png)),
                    StubResult::Fail(message) => Err(anyhow!(message)),
                }
            })
        }
    }

    fn request() -> CaptureRequest {
        CaptureRequest::png(PngProfile::Balanced)
    }

    #[test]
    fn service_requires_at_least_one_backend() {
        assert!(ScreenCaptureService::builder().build().is_err());
    }

    #[tokio::test]
    async fn accepted_backend_stops_pipeline() {
        let first_calls = Arc::new(AtomicUsize::new(0));
        let second_calls = Arc::new(AtomicUsize::new(0));
        let service = ScreenCaptureService::builder()
            .backend(StubBackend::new(
                "first",
                StubResult::Accept(vec![1]),
                first_calls.clone(),
            ))
            .backend(StubBackend::new(
                "second",
                StubResult::Accept(vec![2]),
                second_calls.clone(),
            ))
            .build()
            .unwrap();

        assert_eq!(service.capture(request()).await.unwrap(), vec![1]);
        assert_eq!(first_calls.load(Ordering::SeqCst), 1);
        assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn verification_request_uses_next_backend() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = ScreenCaptureService::builder()
            .backend(StubBackend::new(
                "primary",
                StubResult::Verify(vec![1]),
                calls.clone(),
            ))
            .backend(StubBackend::new(
                "fallback",
                StubResult::Accept(vec![2]),
                calls.clone(),
            ))
            .build()
            .unwrap();

        assert_eq!(service.capture(request()).await.unwrap(), vec![2]);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn backend_error_uses_next_backend() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = ScreenCaptureService::builder()
            .backend(StubBackend::new(
                "primary",
                StubResult::Fail("driver reset"),
                calls.clone(),
            ))
            .backend(StubBackend::new(
                "fallback",
                StubResult::Accept(vec![2]),
                calls.clone(),
            ))
            .build()
            .unwrap();

        assert_eq!(service.capture(request()).await.unwrap(), vec![2]);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn all_failures_include_backend_context() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = ScreenCaptureService::builder()
            .backend(StubBackend::new(
                "primary",
                StubResult::Fail("unavailable"),
                calls.clone(),
            ))
            .backend(StubBackend::new(
                "fallback",
                StubResult::Verify(vec![2]),
                calls,
            ))
            .build()
            .unwrap();

        let error = service.capture(request()).await.unwrap_err().to_string();
        assert!(error.contains("primary: unavailable"));
        assert!(error.contains("fallback"));
    }
}
