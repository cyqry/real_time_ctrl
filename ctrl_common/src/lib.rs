//! `real_ctrl` 与 `ctrl_server` 共用的控制侧协议类型。
//!
//! 本 crate 不建立网络连接：`ctrl_frame` / `ctrl_resp` 定义 TLS 内的消息，`ctrl_protocol` 提供常用
//! 封帧函数，`kik` 则是服务端管理一个在线被控端时使用的并发安全状态对象。

pub mod cmd_resp_info;
pub mod ctrl_frame;
pub mod ctrl_protocol;
pub mod ctrl_resp;
pub mod entity;
pub mod kik;
