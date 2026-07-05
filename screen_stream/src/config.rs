use std::time::Duration;

use crate::error::{Result, ScreenStreamError};
use crate::stats::DebugStatsConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderBackend {
    /// 优先使用 Windows Media Foundation 硬件编码器；如果硬件 H.264 MFT
    /// 初始化失败，则回退到 OpenH264。
    Auto,
    /// 强制使用 Windows Media Foundation H.264 硬件编码路径。
    MediaFoundation,
    /// 强制使用可移植的 OpenH264 软件编码路径。
    OpenH264,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderComplexity {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone)]
pub struct CaptureConfig {
    pub max_fps: u32,
    pub bitrate_bps: u32,
    pub gop_frames: u32,
    pub acquire_timeout_ms: u32,
    pub queue_capacity: usize,
    pub crop_to_even_dimensions: bool,
    pub max_packet_size: usize,
    /// 可选的编码宽度上限。源桌面不会被放大；如果桌面超过该上限，
    /// 会在送入 H.264 前先降采样。
    pub max_encode_width: Option<u32>,
    /// 可选的编码高度上限。通常和 `max_encode_width` 成对设置，
    /// 用来保持桌面宽高比。
    pub max_encode_height: Option<u32>,
    pub encoder_backend: EncoderBackend,
    pub encoder_complexity: EncoderComplexity,
    pub qp_min: u8,
    pub qp_max: u8,
    pub debug_stats: DebugStatsConfig,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self::balanced()
    }
}

impl CaptureConfig {
    pub fn balanced() -> Self {
        Self {
            max_fps: 30,
            bitrate_bps: 8_000_000,
            gop_frames: 60,
            acquire_timeout_ms: 16,
            queue_capacity: 2,
            crop_to_even_dimensions: true,
            max_packet_size: 16 * 1024 * 1024,
            max_encode_width: Some(1920),
            max_encode_height: Some(1080),
            encoder_backend: EncoderBackend::Auto,
            encoder_complexity: EncoderComplexity::Low,
            qp_min: 12,
            qp_max: 38,
            debug_stats: DebugStatsConfig::default(),
        }
    }

    pub fn smooth() -> Self {
        Self {
            max_fps: 60,
            bitrate_bps: 8_000_000,
            gop_frames: 120,
            queue_capacity: 1,
            max_encode_width: Some(1280),
            max_encode_height: Some(720),
            encoder_complexity: EncoderComplexity::Low,
            qp_min: 10,
            qp_max: 38,
            ..Self::balanced()
        }
    }

    pub fn high_quality() -> Self {
        Self {
            max_fps: 30,
            bitrate_bps: 16_000_000,
            gop_frames: 90,
            queue_capacity: 2,
            max_encode_width: Some(1920),
            max_encode_height: Some(1080),
            encoder_complexity: EncoderComplexity::Medium,
            qp_min: 8,
            qp_max: 34,
            ..Self::balanced()
        }
    }

    pub fn bandwidth_saver() -> Self {
        Self {
            max_fps: 30,
            bitrate_bps: 3_000_000,
            gop_frames: 60,
            queue_capacity: 2,
            max_encode_width: Some(1280),
            max_encode_height: Some(720),
            encoder_complexity: EncoderComplexity::Medium,
            qp_min: 16,
            qp_max: 42,
            ..Self::balanced()
        }
    }

    pub fn native_resolution(mut self) -> Self {
        self.max_encode_width = None;
        self.max_encode_height = None;
        self
    }

    pub fn with_encode_bounds(mut self, max_width: u32, max_height: u32) -> Self {
        self.max_encode_width = Some(max_width);
        self.max_encode_height = Some(max_height);
        self
    }

    pub fn with_debug_stats(mut self, enabled: bool) -> Self {
        self.debug_stats.enabled = enabled;
        self
    }

    pub fn with_stats_interval(mut self, interval: Duration) -> Self {
        self.debug_stats.interval = interval;
        self
    }

    pub fn encoded_dimensions(&self, source_width: u32, source_height: u32) -> Result<(u32, u32)> {
        scaled_even_dimensions(
            source_width,
            source_height,
            self.max_encode_width,
            self.max_encode_height,
            self.crop_to_even_dimensions,
        )
    }

    pub fn encoder_config(&self) -> H264EncoderConfig {
        H264EncoderConfig {
            bitrate_bps: self.bitrate_bps,
            max_fps: self.max_fps,
            gop_frames: self.gop_frames,
            allow_skip_frames: true,
            threads: 0,
            max_encode_width: self.max_encode_width,
            max_encode_height: self.max_encode_height,
            crop_to_even_dimensions: self.crop_to_even_dimensions,
            backend: self.encoder_backend,
            complexity: self.encoder_complexity,
            qp_min: self.qp_min,
            qp_max: self.qp_max,
            openh264_debug: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct H264EncoderConfig {
    pub bitrate_bps: u32,
    pub max_fps: u32,
    pub gop_frames: u32,
    pub allow_skip_frames: bool,
    pub threads: u16,
    pub max_encode_width: Option<u32>,
    pub max_encode_height: Option<u32>,
    pub crop_to_even_dimensions: bool,
    pub backend: EncoderBackend,
    pub complexity: EncoderComplexity,
    pub qp_min: u8,
    pub qp_max: u8,
    pub openh264_debug: bool,
}

impl H264EncoderConfig {
    pub fn encoded_dimensions(&self, source_width: u32, source_height: u32) -> Result<(u32, u32)> {
        scaled_even_dimensions(
            source_width,
            source_height,
            self.max_encode_width,
            self.max_encode_height,
            self.crop_to_even_dimensions,
        )
    }
}

impl Default for H264EncoderConfig {
    fn default() -> Self {
        CaptureConfig::default().encoder_config()
    }
}

#[derive(Debug, Clone)]
pub struct PlayerConfig {
    pub max_packet_size: usize,
    pub debug_stats: DebugStatsConfig,
}

impl Default for PlayerConfig {
    fn default() -> Self {
        Self {
            max_packet_size: 16 * 1024 * 1024,
            debug_stats: DebugStatsConfig::default(),
        }
    }
}

impl PlayerConfig {
    pub fn with_debug_stats(mut self, enabled: bool) -> Self {
        self.debug_stats.enabled = enabled;
        self
    }

    pub fn with_stats_interval(mut self, interval: Duration) -> Self {
        self.debug_stats.interval = interval;
        self
    }
}

fn scaled_even_dimensions(
    source_width: u32,
    source_height: u32,
    max_width: Option<u32>,
    max_height: Option<u32>,
    crop_to_even_dimensions: bool,
) -> Result<(u32, u32)> {
    if source_width == 0 || source_height == 0 {
        return Err(ScreenStreamError::InvalidFrame(format!(
            "invalid source dimensions {source_width}x{source_height}"
        )));
    }

    let max_width = max_width.unwrap_or(source_width);
    let max_height = max_height.unwrap_or(source_height);
    if max_width == 0 || max_height == 0 {
        return Err(ScreenStreamError::InvalidFrame(format!(
            "invalid encoded bounds {max_width}x{max_height}"
        )));
    }

    let (mut width, mut height) = if source_width <= max_width && source_height <= max_height {
        (source_width, source_height)
    } else if (source_width as u64) * (max_height as u64)
        > (source_height as u64) * (max_width as u64)
    {
        let width = max_width;
        let height = ((source_height as u64) * (width as u64) / source_width as u64) as u32;
        (width, height.max(1))
    } else {
        let height = max_height;
        let width = ((source_width as u64) * (height as u64) / source_height as u64) as u32;
        (width.max(1), height)
    };

    width = make_even(width, crop_to_even_dimensions)?;
    height = make_even(height, crop_to_even_dimensions)?;
    Ok((width, height))
}

fn make_even(value: u32, crop_to_even_dimensions: bool) -> Result<u32> {
    if value % 2 == 0 {
        return Ok(value);
    }

    if !crop_to_even_dimensions {
        return Err(ScreenStreamError::InvalidFrame(format!(
            "dimension {value} is odd and crop_to_even_dimensions is disabled"
        )));
    }

    let cropped = value - 1;
    if cropped == 0 {
        return Err(ScreenStreamError::InvalidFrame(format!(
            "dimension {value} cannot be cropped to an even value"
        )));
    }
    Ok(cropped)
}
