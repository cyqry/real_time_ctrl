#[cfg(windows)]
pub mod native_window;

#[cfg(windows)]
pub use native_window::{play_from_native_window, NativeWindowConfig};
