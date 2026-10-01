//! 控制端可展示或通过开放 API 返回的结构化服务端数据。
//!
//! 这些类型是业务模型，不直接处理帧、认证或网络 I/O。

use serde::{Deserialize, Serialize};
use std::time::SystemTime;

/// 能力查询由已认证 TLS 主连接返回。缺失字段按不支持处理，调用端不能安全降级。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerCapabilities {
    #[serde(default)]
    pub target_bound_command_v1: bool,
    #[serde(default)]
    pub task_run_v1: bool,
    #[serde(default)]
    pub task_list_v1: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KikInfoVo {
    pub id: String,
    pub name: String,
    pub ip: String,
    pub recent_online_time: SystemTime,
}

/// 服务端进程内保留的 Kik 最近在线状态。
///
/// `recent_offline_unix_ms=None` 表示本进程尚未观察到该 Kik 完整下线；最近记录严格限制为
/// 256 条且不会跨 ctrl_server 重启持久化，避免匿名 Kik 注册导致磁盘状态和历史表无限增长。
/// 时间在线协议/API 中统一为 Unix epoch 毫秒，避免暴露 serde 对 `SystemTime` 的实现结构。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct KikPresenceVo {
    pub id: String,
    pub name: String,
    pub ip: String,
    pub online: bool,
    pub recent_online_unix_ms: u64,
    pub recent_offline_unix_ms: Option<u64>,
}

pub struct Screen {
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// 客户端进程内的当前目标快照；不是服务端会话状态或实时在线证明。
pub enum LocalNow {
    Kik(KikInfoVo),
    None,
    NotOnline,
}
