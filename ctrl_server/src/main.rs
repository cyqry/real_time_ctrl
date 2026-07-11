use common::config::{Config, Id, SecurityConfig};
use core::context::Context;
use core::server;
use std::env;
use std::time::Duration;

mod core;
mod handler;
mod logger;

//编译期获取环境变量，写死在程序
const LOG_LEVEL: &str = env!("LOG_LEVEL");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let bind_host = env::var("CTRL_SERVER_BIND_HOST")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "0.0.0.0".to_string());
    let server_port = env::var("CTRL_SERVER_PORT")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "9002".to_string());

    // 设置全局 INFO 级别
    unsafe {
        std::env::set_var("RUST_LOG", LOG_LEVEL);
    }
    // env_logger::init(); //该库 为 log 库 实现环境变量设置日志级别, 这里应该不需要
    let config = logger::LogConfig {
        dir: std::path::PathBuf::from("./logs"),
        prefix: "ctrl_server".to_string(),
    };
    logger::init_logging_with_config(config)?;
    color_backtrace::install();
    // 进程级别钩子
    // panic::set_hook(Box::new(|panic_info| {
    //     // 获取 backtrace
    //     let backtrace = Backtrace::capture();
    //     error!("panic_info:{:?}", panic_info);
    //
    // }));
    server::run(
        Context::init(),
        Config {
            id: Id::control_plane_from_env("CTRL_SERVER_AUTH_SECRET")?,
            server_host: bind_host,
            server_port,
            read_timeout: Duration::from_secs(45),
            write_timeout: Duration::from_secs(45),
            security: SecurityConfig::ctrl_server_from_env(),
        },
    )
    .await?;
    Ok(())
}
