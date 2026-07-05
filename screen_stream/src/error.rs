use thiserror::Error;

pub type Result<T> = std::result::Result<T, ScreenStreamError>;

#[derive(Debug, Error)]
pub enum ScreenStreamError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid packet: {0}")]
    InvalidPacket(String),

    #[error("unsupported stream version: {0}")]
    UnsupportedVersion(u16),

    #[error("packet length {len} exceeds configured maximum {max}")]
    PacketTooLarge { len: usize, max: usize },

    #[error("codec error: {0}")]
    Codec(#[from] openh264::Error),

    #[cfg(windows)]
    #[error("screen capture error: {0}")]
    Capture(#[from] windows_capture::dxgi_duplication_api::Error),

    #[error("native window playback error: {0}")]
    Window(String),

    #[error("background task failed: {0}")]
    Join(#[from] tokio::task::JoinError),

    #[error("stream channel closed")]
    ChannelClosed,

    #[error("invalid frame: {0}")]
    InvalidFrame(String),
}
