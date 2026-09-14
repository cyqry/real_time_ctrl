//! 服务端核心状态和连接生命周期。
//!
//! `server` 接受并驱动连接，`context` 保存跨连接共享状态，`account` 管理租户策略，
//! `connection_meta` 定义初始化阶段附着到连接上的类型安全属性。

pub mod account;
pub mod connection_meta;
pub mod context;
pub mod server;
