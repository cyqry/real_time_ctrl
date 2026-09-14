//! Kik 命令返回的结构化业务数据。
//!
//! 当前主要承载目录项；结构体先序列化为响应文本，再由控制端恢复为稳定的本地/API 类型。

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Ls {
    pub size: Option<u64>,
    pub filename: Option<String>,
    pub is_file: bool,
    pub created_date: Option<String>,
    pub modified_date: Option<String>,
}
