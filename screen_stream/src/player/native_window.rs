use std::{
    ffi::c_void,
    mem::size_of,
    sync::{
        atomic::{AtomicBool, AtomicIsize, Ordering},
        Arc, Mutex,
    },
};

use tokio::io::AsyncRead;
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{
            BeginPaint, EndPaint, FillRect, GetStockObject, InvalidateRect, SetStretchBltMode,
            StretchDIBits, UpdateWindow, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLACK_BRUSH,
            COLORONCOLOR, DIB_RGB_COLORS, HBRUSH, PAINTSTRUCT, SRCCOPY,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
            GetMessageW, GetWindowLongPtrW, LoadCursorW, PostMessageW, PostQuitMessage,
            RegisterClassW, SetWindowLongPtrW, ShowWindow, TranslateMessage, UnregisterClassW,
            CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, GWLP_USERDATA, IDC_ARROW, MSG,
            SW_SHOW, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND, WM_NCCREATE,
            WM_NCDESTROY, WM_PAINT, WM_SIZE, WNDCLASSW, WS_OVERLAPPEDWINDOW,
        },
    },
};

use crate::{
    codec::DecodedVideoFrame,
    config::PlayerConfig,
    error::{Result, ScreenStreamError},
    play_from,
    stats::RenderDebugStats,
};

const WM_STREAM_FRAME: u32 = WM_APP + 1;

#[derive(Debug, Clone)]
pub struct NativeWindowConfig {
    /// 操作系统窗口标题。
    pub title: String,
    /// 初始外层窗口宽度，单位为物理像素。
    pub initial_width: i32,
    /// 初始外层窗口高度，单位为物理像素。
    pub initial_height: i32,
    /// 用户缩放窗口时保持源画面宽高比，并用黑边填充。
    pub preserve_aspect: bool,
}

impl Default for NativeWindowConfig {
    fn default() -> Self {
        Self {
            title: "Screen Stream Player".to_string(),
            initial_width: 1280,
            initial_height: 720,
            preserve_aspect: true,
        }
    }
}

/// 解码屏幕流，并渲染到真实的 Win32 原生窗口。
///
/// 异步 reader 仍运行在 Tokio 上，窗口消息循环运行在 blocking 线程上。
/// 共享帧槽只保留最新的解码帧，避免慢速绘制或窗口缩放累积播放延迟。
pub async fn play_from_native_window<R>(
    reader: R,
    player_config: PlayerConfig,
    window_config: NativeWindowConfig,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let shared = Arc::new(WindowShared::new(player_config.debug_stats.clone()));
    let stop = Arc::new(AtomicBool::new(false));
    let hwnd_value = Arc::new(AtomicIsize::new(0));

    let decode_shared = Arc::clone(&shared);
    let decode_stop = Arc::clone(&stop);
    let decode_hwnd = Arc::clone(&hwnd_value);
    let mut decode_task = tokio::spawn(async move {
        play_from(reader, player_config, move |frame| {
            if decode_stop.load(Ordering::Acquire) {
                return Err(ScreenStreamError::ChannelClosed);
            }

            decode_shared.store_frame(RenderFrame::from_decoded(frame)?)?;
            decode_shared.request_repaint(&decode_hwnd);
            Ok(())
        })
        .await
    });

    let window_shared = Arc::clone(&shared);
    let window_stop = Arc::clone(&stop);
    let window_hwnd = Arc::clone(&hwnd_value);
    let mut window_task = tokio::task::spawn_blocking(move || {
        run_window_loop(window_config, window_shared, window_stop, window_hwnd)
    });

    tokio::select! {
        window_result = &mut window_task => {
            stop.store(true, Ordering::Release);
            decode_task.abort();
            window_result.map_err(ScreenStreamError::from)??;
            Ok(())
        }
        decode_result = &mut decode_task => {
            stop.store(true, Ordering::Release);
            post_window_message(&hwnd_value, WM_CLOSE);
            let window_result = window_task.await.map_err(ScreenStreamError::from)?;
            window_result?;
            decode_result.map_err(ScreenStreamError::from)??;
            Ok(())
        }
    }
}

struct WindowShared {
    latest: Mutex<Option<Arc<RenderFrame>>>,
    render_stats: Mutex<RenderDebugStats>,
    repaint_pending: AtomicBool,
}

impl WindowShared {
    fn new(debug_stats: crate::DebugStatsConfig) -> Self {
        Self {
            latest: Mutex::new(None),
            render_stats: Mutex::new(RenderDebugStats::new(debug_stats)),
            repaint_pending: AtomicBool::new(false),
        }
    }

    fn store_frame(&self, frame: RenderFrame) -> Result<()> {
        let mut latest = self.latest.lock().map_err(|_| {
            ScreenStreamError::Window("native frame slot mutex was poisoned".to_string())
        })?;
        *latest = Some(Arc::new(frame));
        Ok(())
    }

    fn latest_frame(&self) -> Option<Arc<RenderFrame>> {
        self.latest.lock().ok().and_then(|frame| frame.clone())
    }

    fn record_paint(&self, seq: Option<u64>) {
        self.repaint_pending.store(false, Ordering::Release);
        if let Ok(mut stats) = self.render_stats.lock() {
            stats.on_paint(seq);
        }
    }

    fn request_repaint(&self, hwnd_value: &AtomicIsize) {
        // 解码线程可能比窗口线程快。播放器只保存最新帧，所以这里合并重绘消息：
        // 一个 WM_STREAM_FRAME 尚未被 paint 消费时，不再投递新的重复消息。
        if self
            .repaint_pending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
            && !post_window_message(hwnd_value, WM_STREAM_FRAME)
        {
            self.repaint_pending.store(false, Ordering::Release);
        }
    }
}

struct RenderFrame {
    seq: u64,
    width: u32,
    height: u32,
    bgra: Vec<u8>,
    timestamp_us: u64,
}

impl RenderFrame {
    fn from_decoded(mut frame: DecodedVideoFrame) -> Result<Self> {
        let expected_len = frame
            .width
            .checked_mul(frame.height)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| ScreenStreamError::InvalidFrame("decoded frame size overflows".into()))?
            as usize;

        if frame.rgba.len() != expected_len {
            return Err(ScreenStreamError::InvalidFrame(format!(
                "decoded RGBA buffer has {} bytes, expected {expected_len}",
                frame.rgba.len()
            )));
        }

        // Windows 小端平台上，GDI 32-bit BI_RGB DIB 的实际布局是 BGRA。
        // 解码帧到达时只转换一次，后续绘制可以把同一块内存直接交给
        // StretchDIBits。
        for pixel in frame.rgba.chunks_exact_mut(4) {
            pixel.swap(0, 2);
            pixel[3] = 0xff;
        }

        Ok(Self {
            seq: frame.seq,
            width: frame.width,
            height: frame.height,
            bgra: frame.rgba,
            timestamp_us: frame.timestamp_us,
        })
    }
}

struct WindowState {
    shared: Arc<WindowShared>,
    stop: Arc<AtomicBool>,
    hwnd_value: Arc<AtomicIsize>,
    preserve_aspect: bool,
}

fn run_window_loop(
    config: NativeWindowConfig,
    shared: Arc<WindowShared>,
    stop: Arc<AtomicBool>,
    hwnd_value: Arc<AtomicIsize>,
) -> Result<()> {
    let class_name = wide_null(&format!(
        "ScreenStreamNativeWindow-{:p}",
        Arc::as_ptr(&shared)
    ));
    let title = wide_null(&config.title);
    let mut state = Box::new(WindowState {
        shared,
        stop,
        hwnd_value,
        preserve_aspect: config.preserve_aspect,
    });

    // 所有 Win32 调用都收敛在这个模块内。传给 CreateWindowExW 的 state 指针
    // 在消息循环退出前始终有效，随后 boxed state 会在下面正常 drop。
    unsafe {
        let module = GetModuleHandleW(PCWSTR::null()).map_err(win_error("GetModuleHandleW"))?;
        let instance = HINSTANCE(module.0);
        let wnd_class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            hCursor: LoadCursorW(None, IDC_ARROW).map_err(win_error("LoadCursorW"))?,
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };

        if RegisterClassW(&wnd_class) == 0 {
            return Err(ScreenStreamError::Window(
                "RegisterClassW failed for native player window".to_string(),
            ));
        }

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            config.initial_width.max(320),
            config.initial_height.max(180),
            None,
            None,
            Some(instance),
            Some((&mut *state as *mut WindowState).cast::<c_void>()),
        )
        .map_err(win_error("CreateWindowExW"))?;

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);

        if state.stop.load(Ordering::Acquire) {
            let _ = DestroyWindow(hwnd);
        }

        let mut msg = MSG::default();
        loop {
            let result = GetMessageW(&mut msg, None, 0, 0);
            if result.0 == -1 {
                return Err(ScreenStreamError::Window(
                    "GetMessageW failed in native player window".to_string(),
                ));
            }
            if result.0 == 0 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        state.hwnd_value.store(0, Ordering::Release);
        let _ = UnregisterClassW(PCWSTR(class_name.as_ptr()), Some(instance));
    }

    Ok(())
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let create = lparam.0 as *const CREATESTRUCTW;
            if create.is_null() {
                return LRESULT(0);
            }

            let state = unsafe { (*create).lpCreateParams as *mut WindowState };
            if state.is_null() {
                return LRESULT(0);
            }

            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
                (*state)
                    .hwnd_value
                    .store(hwnd.0 as isize, Ordering::Release);
            }
            LRESULT(1)
        }
        WM_STREAM_FRAME | WM_SIZE => {
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            unsafe {
                paint_window(hwnd);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            if let Some(state) = unsafe { state_from_hwnd(hwnd) } {
                state.stop.store(true, Ordering::Release);
                state.hwnd_value.store(0, Ordering::Release);
            }
            unsafe {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe fn paint_window(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
    let mut client = RECT::default();
    let has_client = unsafe { GetClientRect(hwnd, &mut client).is_ok() };

    if has_client {
        let brush = unsafe { HBRUSH(GetStockObject(BLACK_BRUSH).0) };
        unsafe {
            FillRect(hdc, &client, brush);
        }

        if let Some(state) = unsafe { state_from_hwnd(hwnd) } {
            let mut painted_seq = None;
            if let Some(frame) = state.shared.latest_frame() {
                unsafe {
                    paint_frame(hdc, &client, &frame, state.preserve_aspect);
                }
                painted_seq = Some(frame.seq);
                let _ = frame.timestamp_us;
            }
            state.shared.record_paint(painted_seq);
        }
    }

    unsafe {
        let _ = EndPaint(hwnd, &ps);
    }
}

unsafe fn paint_frame(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    client: &RECT,
    frame: &RenderFrame,
    preserve_aspect: bool,
) {
    let Some(dest) = destination_rect(client, frame.width, frame.height, preserve_aspect) else {
        return;
    };

    let mut bmi = BITMAPINFO::default();
    bmi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bmi.bmiHeader.biWidth = frame.width.min(i32::MAX as u32) as i32;
    bmi.bmiHeader.biHeight = -(frame.height.min(i32::MAX as u32) as i32);
    bmi.bmiHeader.biPlanes = 1;
    bmi.bmiHeader.biBitCount = 32;
    bmi.bmiHeader.biCompression = BI_RGB.0;
    bmi.bmiHeader.biSizeImage = frame.bgra.len().min(u32::MAX as usize) as u32;

    unsafe {
        // COLORONCOLOR 可以避免 HALFTONE 缩放的额外成本。对低延迟桌面流来说，
        // 速度通常比缩放滤镜质量更重要。
        SetStretchBltMode(hdc, COLORONCOLOR);
        StretchDIBits(
            hdc,
            dest.left,
            dest.top,
            dest.right - dest.left,
            dest.bottom - dest.top,
            0,
            0,
            frame.width.min(i32::MAX as u32) as i32,
            frame.height.min(i32::MAX as u32) as i32,
            Some(frame.bgra.as_ptr().cast::<c_void>()),
            &bmi,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
    }
}

fn destination_rect(client: &RECT, width: u32, height: u32, preserve_aspect: bool) -> Option<RECT> {
    let client_width = client.right - client.left;
    let client_height = client.bottom - client.top;
    if client_width <= 0 || client_height <= 0 || width == 0 || height == 0 {
        return None;
    }

    if !preserve_aspect {
        return Some(*client);
    }

    let scale = (client_width as f64 / width as f64).min(client_height as f64 / height as f64);
    let dest_width = ((width as f64 * scale).round() as i32).clamp(1, client_width);
    let dest_height = ((height as f64 * scale).round() as i32).clamp(1, client_height);
    let left = client.left + (client_width - dest_width) / 2;
    let top = client.top + (client_height - dest_height) / 2;

    Some(RECT {
        left,
        top,
        right: left + dest_width,
        bottom: top + dest_height,
    })
}

unsafe fn state_from_hwnd(hwnd: HWND) -> Option<&'static WindowState> {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const WindowState;
    unsafe { ptr.as_ref() }
}

fn post_window_message(hwnd_value: &AtomicIsize, message: u32) -> bool {
    let hwnd = hwnd_value.load(Ordering::Acquire);
    if hwnd == 0 {
        return false;
    }

    // atomic load 之后、PostMessageW 之前窗口可能已经关闭；投递失败只表示
    // 已经没有可重绘的原生 surface。
    unsafe {
        PostMessageW(
            Some(HWND(hwnd as *mut c_void)),
            message,
            WPARAM(0),
            LPARAM(0),
        )
        .is_ok()
    }
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn win_error(
    context: &'static str,
) -> impl FnOnce(windows::core::Error) -> ScreenStreamError + 'static {
    move |error| ScreenStreamError::Window(format!("{context}: {error}"))
}
