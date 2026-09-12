#![cfg_attr(not(target_os = "windows"), allow(dead_code))]
#![deny(unsafe_op_in_unsafe_fn)]

//! DXGI Desktop Duplication 主屏捕获与无损 PNG 编码底层实现。
//!
//! 本模块不管理跨请求生命周期；会话复用、空闲过期和线程所有权由 `dxgi_backend` 负责。

#[cfg(not(target_os = "windows"))]
compile_error!("windows-primary-screen-png only supports Windows 8 or newer");

use std::{error::Error as StdError, fmt, marker::PhantomData, mem::size_of, rc::Rc};

use common::hidden;
use png::{BitDepth, ColorType, Compression, Encoder, Filter};
use windows::{
    core::{Error as WindowsError, Interface},
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::{
                D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_10_0,
                D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
            },
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
                D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_CREATE_DEVICE_SINGLETHREADED, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ,
                D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
            },
            Dxgi::Common::{
                DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION, DXGI_MODE_ROTATION_IDENTITY,
                DXGI_MODE_ROTATION_ROTATE180, DXGI_MODE_ROTATION_ROTATE270,
                DXGI_MODE_ROTATION_ROTATE90, DXGI_MODE_ROTATION_UNSPECIFIED,
            },
            Dxgi::{
                CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1,
                IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST,
                DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET, DXGI_ERROR_NOT_FOUND,
                DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
            },
            Gdi::{GetMonitorInfoW, MONITORINFO},
        },
    },
};

const DEFAULT_ACQUIRE_TIMEOUT_MS: u32 = 750;
const MONITORINFOF_PRIMARY_VALUE: u32 = 0x0000_0001;
const ROTATION_TILE: usize = 32;

/// 底层截屏结果类型。
pub type Result<T> = std::result::Result<T, CaptureError>;

/// PNG 编码档位。所有档位均逐像素无损，仅 CPU 时间和输出体积不同。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PngProfile {
    /// 最低编码延迟，使用快速 DEFLATE 实现和 Sub 行过滤器。
    Fast,
    /// 推荐默认值，使用自适应行过滤平衡体积和延迟。
    #[default]
    Balanced,
    /// 使用更多 CPU 时间换取通常较小的体积下降。
    High,
}

/// 底层采集与编码错误。
#[derive(Debug)]
pub enum CaptureError {
    Windows(WindowsError),
    Png(png::EncodingError),
    NoPrimaryOutput,
    Timeout,
    MissingObject(String),
    UnsupportedPixelFormat(i32),
    UnsupportedRotation(i32),
    InvalidFrame(String),
    ImageTooLarge,
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Windows(error) => f.write_str(&hidden!("Windows API failed: ", error)),
            Self::Png(error) => f.write_str(&hidden!("PNG encoding failed: ", error)),
            Self::NoPrimaryOutput => {
                f.write_str(&hidden!("the Windows primary display could not be found"))
            }
            Self::Timeout => f.write_str(&hidden!("timed out waiting for a desktop frame")),
            Self::MissingObject(name) => f.write_str(&hidden!("Windows returned no ", name)),
            Self::UnsupportedPixelFormat(format) => {
                f.write_str(&hidden!("unsupported DXGI desktop pixel format: ", format))
            }
            Self::UnsupportedRotation(rotation) => {
                f.write_str(&hidden!("unsupported DXGI display rotation: ", rotation))
            }
            Self::InvalidFrame(message) => {
                f.write_str(&hidden!("invalid desktop frame: ", message))
            }
            Self::ImageTooLarge => {
                f.write_str(&hidden!("desktop dimensions exceed addressable memory"))
            }
        }
    }
}

impl StdError for CaptureError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Windows(error) => Some(error),
            Self::Png(error) => Some(error),
            _ => None,
        }
    }
}

impl From<WindowsError> for CaptureError {
    fn from(value: WindowsError) -> Self {
        Self::Windows(value)
    }
}

impl From<png::EncodingError> for CaptureError {
    fn from(value: png::EncodingError) -> Self {
        Self::Png(value)
    }
}

impl CaptureError {
    /// 只有重建 D3D11/DXGI 会话可能恢复的错误才允许外层再尝试一次。
    pub(crate) fn is_rebuildable(&self) -> bool {
        matches!(self, Self::Windows(error) if is_recoverable_duplication_error(error))
    }
}

/// 一次性截取主屏并使用默认 PNG 档位编码。
///
/// 连续截屏应复用 [`PrimaryScreenCapturer`]；每帧重建 D3D11 device 和 duplication
/// 的成本明显高于稳定状态的采集路径。
pub fn capture_primary_screen_png() -> Result<Vec<u8>> {
    capture_primary_screen_png_with_profile(PngProfile::Balanced)
}

/// 一次性截取主屏，并显式指定 PNG 编码档位。
pub fn capture_primary_screen_png_with_profile(profile: PngProfile) -> Result<Vec<u8>> {
    let mut capturer = PrimaryScreenCapturer::new()?;
    capturer.capture_png(profile)
}

/// 仅限单线程使用的主屏捕获会话。
///
/// D3D11 device 使用 `D3D11_CREATE_DEVICE_SINGLETHREADED` 降低驱动同步开销，
/// 因此该值必须在同一线程创建、使用和释放；下方标记会在编译期约束这个契约。
pub struct PrimaryScreenCapturer {
    session: Option<Session>,
    rgb: Vec<u8>,
    cached_dimensions: Option<(u32, u32)>,
    acquire_timeout_ms: u32,
    _single_thread_only: PhantomData<Rc<()>>,
}

impl PrimaryScreenCapturer {
    /// 为 Windows 当前标记的主显示器创建捕获会话。
    pub fn new() -> Result<Self> {
        Ok(Self {
            session: Some(Session::new()?),
            rgb: Vec::new(),
            cached_dimensions: None,
            acquire_timeout_ms: DEFAULT_ACQUIRE_TIMEOUT_MS,
            _single_thread_only: PhantomData,
        })
    }

    /// 修改 `AcquireNextFrame` 的有限等待时间。
    ///
    /// 至少成功采集一帧后，等待超时会复用最近的完整桌面图像。Desktop Duplication
    /// 没有报告桌面更新时这是正确行为，也可避免把静止桌面误判为错误。
    pub fn set_acquire_timeout_ms(&mut self, timeout_ms: u32) {
        self.acquire_timeout_ms = timeout_ms;
    }

    /// 捕获主显示器并返回完整 PNG 文件字节。
    ///
    /// 高频循环应优先使用 [`Self::capture_png_into`]，让调用者复用 PNG 缓冲分配。
    pub fn capture_png(&mut self, profile: PngProfile) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.capture_png_into(profile, &mut bytes)?;
        Ok(bytes)
    }

    /// 捕获到调用者提供的字节缓冲，跨调用复用其内存分配。
    ///
    /// 成功时 `output` 只包含一个完整 PNG；失败时缓冲会清空，避免发布残缺 PNG。
    pub fn capture_png_into(&mut self, profile: PngProfile, output: &mut Vec<u8>) -> Result<()> {
        // 即使采集在编码前失败，也保持失败后不残留旧 PNG 的接口约束。
        output.clear();
        let is_first_capture = self.cached_dimensions.is_none();
        let (mut width, mut height) = self.capture_dimensions_with_recovery()?;

        // 部分显卡驱动会把 DuplicateOutput 的首个资源初始化为全零，下一帧才是完整桌面。
        // 只在会话首帧触发一次重取，避免给稳定热路径增加等待和内存扫描。
        if is_first_capture && is_uniform_black(&self.rgb) {
            (width, height) = self.capture_dimensions_with_recovery()?;
        }
        encode_png_into(&self.rgb, width, height, profile, output)
    }

    /// 重新枚举适配器并绑定 Windows 当前标记的主显示器。
    ///
    /// 普通模式切换和设备重置会自动恢复；应用收到显示拓扑通知并希望立即切换时，
    /// 可以显式调用本方法。
    pub fn refresh_primary_output(&mut self) -> Result<()> {
        // 每个进程对同一输出只能持有一个 duplication 接口。必须先释放旧接口再创建新接口；
        // 若先构造再赋值，旧对象在构造期间仍存活，同一输出可能返回 E_INVALIDARG。
        self.session = None;
        self.rgb.clear();
        self.cached_dimensions = None;
        self.session = Some(Session::new()?);
        Ok(())
    }

    /// 最近一次采集并校正旋转后的图像尺寸。
    pub fn cached_dimensions(&self) -> Option<(u32, u32)> {
        self.cached_dimensions
    }

    /// 返回最近完整帧是否为全黑。
    pub fn last_frame_is_uniform_black(&self) -> bool {
        self.cached_dimensions.is_some() && is_uniform_black(&self.rgb)
    }

    /// 不获取新桌面帧，重新编码最近一次采集结果。
    ///
    /// 用于在相同像素输入上比较不同无损压缩档位。
    pub fn encode_cached_png_into(&self, profile: PngProfile, output: &mut Vec<u8>) -> Result<()> {
        let (width, height) = self
            .cached_dimensions
            .ok_or_else(|| CaptureError::MissingObject(hidden!("cached desktop frame")))?;
        encode_png_into(&self.rgb, width, height, profile, output)
    }

    fn capture_dimensions_with_recovery(&mut self) -> Result<(u32, u32)> {
        if self.session.is_none() {
            self.refresh_primary_output()?;
        }

        let mut rebuilt = false;
        loop {
            match self.capture_rgb_once() {
                Ok(dimensions) => return Ok(dimensions),
                Err(CaptureError::Timeout) => {
                    return self.cached_dimensions.ok_or(CaptureError::Timeout);
                }
                Err(CaptureError::Windows(error))
                    if !rebuilt && is_recoverable_duplication_error(&error) =>
                {
                    // 显示模式或桌面切换、锁屏恢复、睡眠唤醒、显卡重置都可能导致访问丢失。
                    // 此时重新枚举，不能假设主输出仍连接在原适配器上。
                    self.refresh_primary_output()?;
                    rebuilt = true;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn capture_rgb_once(&mut self) -> Result<(u32, u32)> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| CaptureError::MissingObject(hidden!("capture session")))?;
        let frame = match AcquiredFrame::acquire(&session.duplication, self.acquire_timeout_ms)? {
            Some(frame) => frame,
            None => return Err(CaptureError::Timeout),
        };

        let desktop_texture: ID3D11Texture2D = frame.resource()?.cast()?;
        let mut source_desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { desktop_texture.GetDesc(&mut source_desc) };

        if source_desc.Width == 0 || source_desc.Height == 0 {
            return Err(CaptureError::InvalidFrame(hidden!("zero-sized texture")));
        }
        if source_desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(CaptureError::UnsupportedPixelFormat(source_desc.Format.0));
        }
        if source_desc.ArraySize != 1 || source_desc.MipLevels != 1 {
            return Err(CaptureError::InvalidFrame(hidden!(
                "Desktop Duplication returned an unexpected texture layout"
            )));
        }
        if source_desc.SampleDesc.Count != 1 || source_desc.SampleDesc.Quality != 0 {
            return Err(CaptureError::InvalidFrame(hidden!(
                "Desktop Duplication returned an unexpected multisampled texture"
            )));
        }

        session.ensure_staging(&source_desc)?;
        let staging = session
            .staging
            .as_ref()
            .cloned()
            .ok_or_else(|| CaptureError::MissingObject(hidden!("D3D11 staging texture")))?;
        let context = session.context.clone();

        unsafe { context.CopyResource(&staging, &desktop_texture) };

        // Map 不使用 D3D11_MAP_FLAG_DO_NOT_WAIT，等待 GPU 复制完成后才释放 duplication surface。
        // ReleaseFrame 后原 surface 不再可用于 DirectX 操作，后续 CPU 转换只访问私有 staging texture。
        drop(desktop_texture);
        let mapped = MappedTexture::map(context, staging)?;
        drop(frame);

        let (width, height) = copy_bgra_to_rgb_rotated(
            mapped.data(),
            mapped.row_pitch(),
            source_desc.Width,
            source_desc.Height,
            session.rotation,
            &mut self.rgb,
        )?;

        self.cached_dimensions = Some((width, height));
        Ok((width, height))
    }
}

struct Session {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    rotation: DXGI_MODE_ROTATION,
    staging: Option<ID3D11Texture2D>,
    staging_key: Option<StagingKey>,
}

impl Session {
    fn new() -> Result<Self> {
        let (adapter, output) = find_primary_output()?;
        let (device, context) = create_d3d11_device(&adapter)?;
        let duplication = unsafe { output.DuplicateOutput(&device)? };
        let duplication_desc = unsafe { duplication.GetDesc() };

        if duplication_desc.ModeDesc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(CaptureError::UnsupportedPixelFormat(
                duplication_desc.ModeDesc.Format.0,
            ));
        }
        validate_rotation(duplication_desc.Rotation)?;

        Ok(Self {
            device,
            context,
            duplication,
            rotation: duplication_desc.Rotation,
            staging: None,
            staging_key: None,
        })
    }

    fn ensure_staging(&mut self, source_desc: &D3D11_TEXTURE2D_DESC) -> Result<()> {
        let key = StagingKey::from(source_desc);
        if self.staging_key == Some(key) && self.staging.is_some() {
            return Ok(());
        }

        let mut staging_desc = *source_desc;
        staging_desc.Usage = D3D11_USAGE_STAGING;
        staging_desc.BindFlags = 0;
        staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        staging_desc.MiscFlags = 0;

        let mut texture = None;
        unsafe {
            self.device
                .CreateTexture2D(&staging_desc, None, Some(&mut texture))?;
        }

        self.staging = Some(
            texture.ok_or_else(|| CaptureError::MissingObject(hidden!("D3D11 staging texture")))?,
        );
        self.staging_key = Some(key);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StagingKey {
    width: u32,
    height: u32,
    mip_levels: u32,
    array_size: u32,
    format: i32,
    sample_count: u32,
    sample_quality: u32,
}

impl From<&D3D11_TEXTURE2D_DESC> for StagingKey {
    fn from(desc: &D3D11_TEXTURE2D_DESC) -> Self {
        Self {
            width: desc.Width,
            height: desc.Height,
            mip_levels: desc.MipLevels,
            array_size: desc.ArraySize,
            format: desc.Format.0,
            sample_count: desc.SampleDesc.Count,
            sample_quality: desc.SampleDesc.Quality,
        }
    }
}

fn find_primary_output() -> Result<(IDXGIAdapter1, IDXGIOutput1)> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut origin_fallback: Option<(IDXGIAdapter1, IDXGIOutput1)> = None;
    let mut adapter_index = 0u32;

    loop {
        let adapter = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(adapter) => adapter,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => return Err(error.into()),
        };

        let mut output_index = 0u32;
        loop {
            let output = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(output) => output,
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(error) => return Err(error.into()),
            };

            let desc = unsafe { output.GetDesc()? };
            if desc.AttachedToDesktop.as_bool() {
                let output1: IDXGIOutput1 = output.cast()?;

                let mut monitor_info = MONITORINFO {
                    cbSize: size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                let got_monitor_info =
                    unsafe { GetMonitorInfoW(desc.Monitor, &mut monitor_info).as_bool() };
                if got_monitor_info && (monitor_info.dwFlags & MONITORINFOF_PRIMARY_VALUE) != 0 {
                    return Ok((adapter, output1));
                }

                // Windows 规定主显示器桌面原点为 (0, 0)；仅在异常驱动导致 GetMonitorInfoW
                // 失败时，把该规则作为防御性兜底。
                if origin_fallback.is_none()
                    && desc.DesktopCoordinates.left == 0
                    && desc.DesktopCoordinates.top == 0
                {
                    origin_fallback = Some((adapter.clone(), output1));
                }
            }

            output_index = output_index
                .checked_add(1)
                .ok_or(CaptureError::ImageTooLarge)?;
        }

        adapter_index = adapter_index
            .checked_add(1)
            .ok_or(CaptureError::ImageTooLarge)?;
    }

    origin_fallback.ok_or(CaptureError::NoPrimaryOutput)
}

fn create_d3d11_device(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    const FEATURE_LEVELS: [D3D_FEATURE_LEVEL; 4] = [
        D3D_FEATURE_LEVEL_11_1,
        D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_10_1,
        D3D_FEATURE_LEVEL_10_0,
    ];

    let mut device = None;
    let mut context = None;
    let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_SINGLETHREADED;

    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            flags,
            Some(&FEATURE_LEVELS),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
    }

    Ok((
        device.ok_or_else(|| CaptureError::MissingObject(hidden!("D3D11 device")))?,
        context.ok_or_else(|| CaptureError::MissingObject(hidden!("D3D11 immediate context")))?,
    ))
}

struct AcquiredFrame {
    duplication: IDXGIOutputDuplication,
    resource: Option<IDXGIResource>,
}

impl AcquiredFrame {
    fn acquire(duplication: &IDXGIOutputDuplication, timeout_ms: u32) -> Result<Option<Self>> {
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;

        match unsafe { duplication.AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource) } {
            Ok(()) => {}
            Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(error) => return Err(error.into()),
        }

        if resource.is_none() {
            // AcquireNextFrame 已成功，即使 API 返回无效空资源也必须调用 ReleaseFrame。
            let _ = unsafe { duplication.ReleaseFrame() };
            return Err(CaptureError::MissingObject(hidden!(
                "DXGI desktop resource"
            )));
        }

        Ok(Some(Self {
            duplication: duplication.clone(),
            resource,
        }))
    }

    fn resource(&self) -> Result<&IDXGIResource> {
        self.resource
            .as_ref()
            .ok_or_else(|| CaptureError::MissingObject(hidden!("DXGI desktop resource")))
    }
}

impl Drop for AcquiredFrame {
    fn drop(&mut self) {
        // 先释放帧资源引用，再把帧所有权归还给 Desktop Duplication。
        self.resource.take();
        let _ = unsafe { self.duplication.ReleaseFrame() };
    }
}

struct MappedTexture {
    context: ID3D11DeviceContext,
    texture: ID3D11Texture2D,
    mapped: D3D11_MAPPED_SUBRESOURCE,
}

impl MappedTexture {
    fn map(context: ID3D11DeviceContext, texture: ID3D11Texture2D) -> Result<Self> {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            context.Map(&texture, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }

        if mapped.pData.is_null() || mapped.RowPitch == 0 {
            unsafe { context.Unmap(&texture, 0) };
            return Err(CaptureError::InvalidFrame(hidden!(
                "D3D11 Map returned a null address or zero row pitch"
            )));
        }

        Ok(Self {
            context,
            texture,
            mapped,
        })
    }

    fn data(&self) -> *const u8 {
        self.mapped.pData.cast::<u8>()
    }

    fn row_pitch(&self) -> usize {
        self.mapped.RowPitch as usize
    }
}

impl Drop for MappedTexture {
    fn drop(&mut self) {
        unsafe { self.context.Unmap(&self.texture, 0) };
    }
}

fn is_recoverable_duplication_error(error: &WindowsError) -> bool {
    let code = error.code();
    code == DXGI_ERROR_ACCESS_LOST
        || code == DXGI_ERROR_DEVICE_REMOVED
        || code == DXGI_ERROR_DEVICE_RESET
}

fn validate_rotation(rotation: DXGI_MODE_ROTATION) -> Result<()> {
    match rotation {
        DXGI_MODE_ROTATION_UNSPECIFIED
        | DXGI_MODE_ROTATION_IDENTITY
        | DXGI_MODE_ROTATION_ROTATE90
        | DXGI_MODE_ROTATION_ROTATE180
        | DXGI_MODE_ROTATION_ROTATE270 => Ok(()),
        other => Err(CaptureError::UnsupportedRotation(other.0)),
    }
}

fn output_dimensions(
    width: usize,
    height: usize,
    rotation: DXGI_MODE_ROTATION,
) -> Result<(usize, usize)> {
    validate_rotation(rotation)?;
    if rotation == DXGI_MODE_ROTATION_ROTATE90 || rotation == DXGI_MODE_ROTATION_ROTATE270 {
        Ok((height, width))
    } else {
        Ok((width, height))
    }
}

fn copy_bgra_to_rgb_rotated(
    source: *const u8,
    row_pitch: usize,
    source_width: u32,
    source_height: u32,
    rotation: DXGI_MODE_ROTATION,
    destination: &mut Vec<u8>,
) -> Result<(u32, u32)> {
    if source.is_null() {
        return Err(CaptureError::InvalidFrame(hidden!("null source address")));
    }

    let width = source_width as usize;
    let height = source_height as usize;
    if width == 0 || height == 0 {
        return Err(CaptureError::InvalidFrame(hidden!("zero-sized image")));
    }

    let minimum_row = width.checked_mul(4).ok_or(CaptureError::ImageTooLarge)?;
    if row_pitch < minimum_row {
        return Err(CaptureError::InvalidFrame(hidden!(
            "mapped row pitch is smaller than the BGRA row"
        )));
    }

    // 进入无边界检查的热循环前，先验证所有偏移计算。
    let _source_span = row_pitch
        .checked_mul(height.saturating_sub(1))
        .and_then(|offset| offset.checked_add(minimum_row))
        .ok_or(CaptureError::ImageTooLarge)?;

    let (output_width, output_height) = output_dimensions(width, height, rotation)?;
    let output_len = output_width
        .checked_mul(output_height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or(CaptureError::ImageTooLarge)?;
    destination.resize(output_len, 0);

    unsafe {
        if rotation == DXGI_MODE_ROTATION_UNSPECIFIED || rotation == DXGI_MODE_ROTATION_IDENTITY {
            copy_identity(source, row_pitch, width, height, destination.as_mut_ptr());
        } else if rotation == DXGI_MODE_ROTATION_ROTATE180 {
            copy_rotate_180(source, row_pitch, width, height, destination.as_mut_ptr());
        } else if rotation == DXGI_MODE_ROTATION_ROTATE90 {
            copy_rotate_90_tiled(source, row_pitch, width, height, destination.as_mut_ptr());
        } else if rotation == DXGI_MODE_ROTATION_ROTATE270 {
            copy_rotate_270_tiled(source, row_pitch, width, height, destination.as_mut_ptr());
        } else {
            return Err(CaptureError::UnsupportedRotation(rotation.0));
        }
    }

    Ok((
        u32::try_from(output_width).map_err(|_| CaptureError::ImageTooLarge)?,
        u32::try_from(output_height).map_err(|_| CaptureError::ImageTooLarge)?,
    ))
}

#[inline]
fn is_uniform_black(rgb: &[u8]) -> bool {
    !rgb.is_empty() && rgb.iter().all(|channel| *channel == 0)
}

#[inline(always)]
unsafe fn copy_pixel_bgra_to_rgb(source: *const u8, destination: *mut u8) {
    // 安全性：所有调用者进入循环前都已验证完整的源和目标内存范围。
    unsafe {
        *destination = *source.add(2);
        *destination.add(1) = *source.add(1);
        *destination.add(2) = *source;
    }
}

unsafe fn copy_identity(
    source: *const u8,
    row_pitch: usize,
    width: usize,
    height: usize,
    destination: *mut u8,
) {
    unsafe {
        for y in 0..height {
            let source_row = source.add(y * row_pitch);
            let destination_row = destination.add(y * width * 3);
            for x in 0..width {
                copy_pixel_bgra_to_rgb(source_row.add(x * 4), destination_row.add(x * 3));
            }
        }
    }
}

unsafe fn copy_rotate_180(
    source: *const u8,
    row_pitch: usize,
    width: usize,
    height: usize,
    destination: *mut u8,
) {
    unsafe {
        for destination_y in 0..height {
            let source_y = height - 1 - destination_y;
            let source_row = source.add(source_y * row_pitch);
            let destination_row = destination.add(destination_y * width * 3);
            for destination_x in 0..width {
                let source_x = width - 1 - destination_x;
                copy_pixel_bgra_to_rgb(
                    source_row.add(source_x * 4),
                    destination_row.add(destination_x * 3),
                );
            }
        }
    }
}

unsafe fn copy_rotate_90_tiled(
    source: *const u8,
    row_pitch: usize,
    width: usize,
    height: usize,
    destination: *mut u8,
) {
    let output_width = height;
    unsafe {
        for x0 in (0..width).step_by(ROTATION_TILE) {
            let x1 = (x0 + ROTATION_TILE).min(width);
            for y0 in (0..height).step_by(ROTATION_TILE) {
                let y1 = (y0 + ROTATION_TILE).min(height);
                for x in x0..x1 {
                    let destination_row = destination.add(x * output_width * 3);
                    for y in y0..y1 {
                        let destination_x = height - 1 - y;
                        copy_pixel_bgra_to_rgb(
                            source.add(y * row_pitch + x * 4),
                            destination_row.add(destination_x * 3),
                        );
                    }
                }
            }
        }
    }
}

unsafe fn copy_rotate_270_tiled(
    source: *const u8,
    row_pitch: usize,
    width: usize,
    height: usize,
    destination: *mut u8,
) {
    let output_width = height;
    unsafe {
        for x0 in (0..width).step_by(ROTATION_TILE) {
            let x1 = (x0 + ROTATION_TILE).min(width);
            for y0 in (0..height).step_by(ROTATION_TILE) {
                let y1 = (y0 + ROTATION_TILE).min(height);
                for x in x0..x1 {
                    let destination_y = width - 1 - x;
                    let destination_row = destination.add(destination_y * output_width * 3);
                    for y in y0..y1 {
                        copy_pixel_bgra_to_rgb(
                            source.add(y * row_pitch + x * 4),
                            destination_row.add(y * 3),
                        );
                    }
                }
            }
        }
    }
}

fn encode_png(rgb: &[u8], width: u32, height: u32, profile: PngProfile) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    encode_png_into(rgb, width, height, profile, &mut bytes)?;
    Ok(bytes)
}

fn encode_png_into(
    rgb: &[u8],
    width: u32,
    height: u32,
    profile: PngProfile,
    bytes: &mut Vec<u8>,
) -> Result<()> {
    // 预先建立接口约束：任何错误都不能让 `bytes` 留下残缺或过期 PNG。
    bytes.clear();
    if width == 0 || height == 0 {
        return Err(CaptureError::InvalidFrame(hidden!(
            "cannot encode a zero-sized image"
        )));
    }

    let expected_len = (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or(CaptureError::ImageTooLarge)?;
    if rgb.len() != expected_len {
        return Err(CaptureError::InvalidFrame(hidden!(
            "RGB buffer length does not match the PNG dimensions"
        )));
    }

    // 多数桌面图像压缩率较高。预留量故意低于原始 RGB 大小，既减少常见扩容，
    // 也不强迫调用者为易压缩界面长期保留另一份完整帧内存。
    bytes.reserve((rgb.len() / 3).saturating_add(4096));

    let result = (|| -> Result<()> {
        let mut encoder = Encoder::new(&mut *bytes, width, height);
        encoder.set_color(ColorType::Rgb);
        encoder.set_depth(BitDepth::Eight);

        // 压缩档位不改变像素。Desktop Duplication 的 alpha 对桌面截图没有有效透明度信息，
        // 使用 RGB 可避免给过滤器增加 33% 的无用输入。
        match profile {
            PngProfile::Fast => {
                encoder.set_compression(Compression::Fast);
                encoder.set_filter(Filter::Sub);
            }
            PngProfile::Balanced => {
                encoder.set_compression(Compression::Balanced);
                encoder.set_filter(Filter::Adaptive);
            }
            PngProfile::High => {
                encoder.set_compression(Compression::High);
                encoder.set_filter(Filter::Adaptive);
            }
        }

        let mut writer = encoder.write_header()?;
        writer.write_image_data(rgb)?;
        writer.finish()?;
        Ok(())
    })();

    if result.is_err() {
        bytes.clear();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source_3x2() -> Vec<u8> {
        // 顺序为 B、G、R、A；红色通道保存便于核对的像素编号 1..=6。
        vec![
            0, 0, 1, 255, 0, 0, 2, 255, 0, 0, 3, 255, 0, 0, 4, 255, 0, 0, 5, 255, 0, 0, 6, 255,
        ]
    }

    fn red_ids(rgb: &[u8]) -> Vec<u8> {
        rgb.chunks_exact(3).map(|pixel| pixel[0]).collect()
    }

    fn convert(rotation: DXGI_MODE_ROTATION) -> ((u32, u32), Vec<u8>) {
        let source = source_3x2();
        let mut rgb = Vec::new();
        let dimensions =
            copy_bgra_to_rgb_rotated(source.as_ptr(), 3 * 4, 3, 2, rotation, &mut rgb).unwrap();
        (dimensions, red_ids(&rgb))
    }

    #[test]
    fn identity_rotation() {
        assert_eq!(
            convert(DXGI_MODE_ROTATION_IDENTITY),
            ((3, 2), vec![1, 2, 3, 4, 5, 6])
        );
    }

    #[test]
    fn rotation_90_clockwise() {
        assert_eq!(
            convert(DXGI_MODE_ROTATION_ROTATE90),
            ((2, 3), vec![4, 1, 5, 2, 6, 3])
        );
    }

    #[test]
    fn rotation_180() {
        assert_eq!(
            convert(DXGI_MODE_ROTATION_ROTATE180),
            ((3, 2), vec![6, 5, 4, 3, 2, 1])
        );
    }

    #[test]
    fn rotation_270_clockwise() {
        assert_eq!(
            convert(DXGI_MODE_ROTATION_ROTATE270),
            ((2, 3), vec![3, 6, 2, 5, 1, 4])
        );
    }

    #[test]
    fn png_is_complete_and_has_signature() {
        let rgb = vec![255, 0, 0, 0, 255, 0];
        let encoded = encode_png(&rgb, 2, 1, PngProfile::Fast).unwrap();
        assert_eq!(&encoded[..8], b"\x89PNG\r\n\x1a\n");
        assert!(encoded.ends_with(&[0xAE, 0x42, 0x60, 0x82]));
    }

    #[test]
    fn bgra_channels_and_row_pitch_are_handled() {
        // 每行一个可见像素，后跟四字节驱动填充。
        let source = vec![
            10, 20, 30, 255, 0xAA, 0xAA, 0xAA, 0xAA, 40, 50, 60, 255, 0xBB, 0xBB, 0xBB, 0xBB,
        ];
        let mut rgb = Vec::new();
        let dimensions = copy_bgra_to_rgb_rotated(
            source.as_ptr(),
            8,
            1,
            2,
            DXGI_MODE_ROTATION_IDENTITY,
            &mut rgb,
        )
        .unwrap();

        assert_eq!(dimensions, (1, 2));
        assert_eq!(rgb, vec![30, 20, 10, 60, 50, 40]);
    }

    #[test]
    fn png_error_clears_existing_output() {
        let mut encoded = vec![1, 2, 3, 4];
        let error =
            encode_png_into(&[255, 0, 0], 2, 1, PngProfile::Fast, &mut encoded).unwrap_err();
        assert!(matches!(error, CaptureError::InvalidFrame(_)));
        assert!(encoded.is_empty());
    }

    #[test]
    fn png_into_reuses_and_replaces_the_output() {
        let rgb = vec![255, 0, 0, 0, 255, 0];
        let mut encoded = vec![1, 2, 3, 4];
        encode_png_into(&rgb, 2, 1, PngProfile::Balanced, &mut encoded).unwrap();
        assert_eq!(&encoded[..8], b"\x89PNG\r\n\x1a\n");
        assert!(encoded.ends_with(&[0xAE, 0x42, 0x60, 0x82]));
    }

    #[test]
    fn uniform_black_detection_is_exact() {
        assert!(!is_uniform_black(&[]));
        assert!(is_uniform_black(&[0, 0, 0, 0, 0, 0]));
        assert!(!is_uniform_black(&[0, 0, 0, 0, 0, 1]));
    }

    #[test]
    fn only_transient_device_errors_are_rebuildable() {
        assert!(
            CaptureError::Windows(WindowsError::from_hresult(DXGI_ERROR_ACCESS_LOST))
                .is_rebuildable()
        );
        assert!(
            CaptureError::Windows(WindowsError::from_hresult(DXGI_ERROR_DEVICE_REMOVED))
                .is_rebuildable()
        );
        assert!(!CaptureError::Timeout.is_rebuildable());
        assert!(!CaptureError::UnsupportedPixelFormat(0).is_rebuildable());
    }
}
