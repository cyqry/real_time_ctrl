//! 服务端保存的 Kik 展示信息。
//!
//! 可变字段使用共享异步锁，使主连接重连时能原地更新 IP 和上线时间，而无需复制或替换整个 Kik 会话。

use common::kik_info::KikInfo;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::RwLock;

#[derive(Clone)]
/// 协议注册信息加上服务端观察到的网络和时间信息。
pub struct KikClientInfo {
    pub kik_info: KikInfo,
    pub ip: Arc<RwLock<String>>,
    pub recent_online_time: Arc<RwLock<SystemTime>>,
}
