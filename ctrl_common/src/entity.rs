use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::RwLock;
use common::kik_info::KikInfo;

#[derive(Clone)]
pub struct KikClientInfo {
    pub kik_info: KikInfo,
    pub ip: Arc<RwLock<String>>,
    pub recent_online_time: Arc<RwLock<SystemTime>>,
}
