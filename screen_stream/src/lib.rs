//! 面向 Windows 桌面共享的低延迟屏幕流基础能力。
//!
//! 当前实现优先使用 DXGI Desktop Duplication，而不是 Windows Graphics
//! Capture，因此用户会话里不会出现选择器、授权弹窗或采集边框。浏览器播放
//! 设计成同一份 H.264 编码包上的传输/展示适配层，细节见
//! `docs/browser_playback.md`。

pub mod capture;
pub mod codec;
pub mod config;
pub mod error;
pub mod player;
pub mod stats;
pub mod stream;
pub mod wire;

#[cfg(windows)]
pub use codec::RawD3D11Frame;
pub use codec::{DecodedVideoFrame, EncodedVideoFrame, RawBgraFrame};
pub use config::{
    CaptureConfig, EncoderBackend, EncoderComplexity, H264EncoderConfig, PlayerConfig,
};
pub use error::{Result, ScreenStreamError};
pub use stats::DebugStatsConfig;
pub use stream::{
    play_from, read_packet, read_packet_counted, receive_decoded, write_packet,
    write_packet_counted,
};
pub use wire::{CodecId, EncodedPacket, StreamInfo, WirePacket};

#[cfg(windows)]
pub use capture::dxgi::send_primary_screen;

#[cfg(windows)]
pub use player::{play_from_native_window, NativeWindowConfig};
