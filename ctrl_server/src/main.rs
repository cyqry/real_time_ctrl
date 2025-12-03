use core::context::Context;
use common::config::{Config, Id};
use std::{env, panic, thread};
use std::backtrace::Backtrace;
use std::time::Duration;
use log::{debug, error, info, warn};
use tracing_log::LogTracer;
use core::server;

mod core;
mod handler;
mod logger;

#[tokio::main]
async fn main() {

    // 设置全局 INFO 级别，但模块 `core` 设置为 DEBUG 级别。
    unsafe { std::env::set_var("RUST_LOG", "info"); }
    // env_logger::init(); //该库 为 log 库 实现环境变量设置日志级别, 这里应该不需要
    let config = logger::LogConfig {
        dir: std::path::PathBuf::from("./logs"),
        prefix: "ctrl_server".to_string(),
        ..Default::default()
    };
    logger::init_logging_with_config(config).unwrap();
    color_backtrace::install();
    // 进程级别钩子
    // panic::set_hook(Box::new(|panic_info| {
    //     // 获取 backtrace
    //     let backtrace = Backtrace::capture();
    //     error!("panic_info:{:?}", panic_info);
    //
    // }));

    match server::run(
        Context::init(),
        Config {
            id: Id {
                username: "root".to_string(),
                password: "1104399".to_string(),
            },
            server_host: "0.0.0.0".to_string(),
            server_port: "9002".to_string(),
            read_timeout: Duration::from_secs(45),
            write_timeout: Duration::from_secs(45),
        },
    )
        .await {
        Ok(_) => {}
        Err(e) => {
            error!("服务停止!{}", e);
        }
    };
}

#[tokio::test]
async fn tets() {
    let data: Vec<(Option<String>, bool, u64, Option<String>, Option<String>)> = vec![
        (
            Some("file1.txt".to_string()),
            true,
            1024,
            Some("2021-01-01".to_string()),
            Some("2021-01-02".to_string()),
        ),
        (
            Some("file2_with_long_name.txt".to_string()),
            false,
            2048,
            None,
            Some("2021-01-03".to_string()),
        ),
        (
            None,
            false,
            4096,
            Some("2021-01-04".to_string()),
            Some("2021-01-05".to_string()),
        ),
    ];
}
