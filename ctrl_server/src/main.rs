use core::context::Context;
use common::config::{Config, Id};
use std::env;
use std::time::Duration;
use core::server;

mod core;
mod handler;

#[tokio::main]
async fn main() {
    env::set_var("RUST_LOG", "DEBUG");
    env_logger::init(); //该库 为 log 库 实现环境变量设置日志级别
    server::run(
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
    .await
    .unwrap();
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
