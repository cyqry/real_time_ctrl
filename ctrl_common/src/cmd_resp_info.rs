use serde::{Deserialize, Serialize};
use std::time::{Instant, SystemTime};

#[derive(Clone, Debug,Serialize,Deserialize)]
pub struct KikInfoVo {
    pub id: String,
    pub name: String,
    pub ip: String,
    pub recent_online_time: SystemTime,
}

pub struct Screen {
    pub data: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SysNow {
    Kik(KikInfoVo),
    None,
    NotOnline,
}
