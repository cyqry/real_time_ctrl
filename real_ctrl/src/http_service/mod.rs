//! HTTP 适配层：路由、token 鉴权、响应转换和统一错误映射。
//!
//! 这里不直接操作远程连接，所有业务请求必须进入 `RealCtrlApi`。

pub mod error;
pub mod handlers;
pub mod routes;
