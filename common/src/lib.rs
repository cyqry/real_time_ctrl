//! 三端共享的基础能力。
//!
//! 新维护者可以把本 crate 分成四层理解：`secure_transport` / `noise_transport` 保护字节流，
//! `ltc_codec` / `protocol` 划分帧，`command` / `message` 定义业务消息，`file_util` 提供
//! 受限的文件 I/O。业务 crate 应复用这些入口，不要各自实现一套线协议。

extern crate self as common;

pub mod channel;
pub mod command;
pub mod config;
pub mod file_util;
pub mod kik_info;
pub mod ltc_codec;
pub mod message;
pub mod noise_transport;
pub mod protocol;
pub mod secure_transport;
pub mod session_auth;
pub mod string_obfuscation;
pub mod time_util;

pub use string_obfuscation_macros::hidden;

pub mod generated;
pub mod host;
