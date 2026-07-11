//! real_ctrl 的稳定业务与开放 API 组合根。
//!
//! 三个二进制只负责进程形态和启动参数，连接、策略、管道与 HTTP 适配统一在本库编译一次，
//! 避免过去每个 bin 重复声明同一批模块、重复运行测试并逐渐产生行为分叉。

pub mod api_contract;
pub mod api_service;
pub mod context;
pub mod ctrl_conn;
pub mod ctrl_data_conn;
pub mod ctrl_executor;
pub mod direct_executor;
pub mod dispatch;
pub mod http_service;
pub mod input_command;
pub mod local_client;
pub mod local_executor;
pub mod local_server;
pub mod pipe;
pub mod run_util;
pub mod server_executor;
