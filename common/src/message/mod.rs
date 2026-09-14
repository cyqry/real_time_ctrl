//! 远程协议的业务消息集合。
//!
//! `init_frame` 只用于连接分型与认证，`kik_frame` / `kik_resp` 用于 Kik 业务通道，`dok` 是文件数据
//! payload。所有解析器都接收已经由长度解码器切好的单帧 body。

pub mod dok;
pub mod init_frame;
pub mod kik_cmd_resp_info;
pub mod kik_frame;
pub mod kik_resp;
