#![windows_subsystem = "windows"]

use crate::api_service::RealCtrlApi;
use crate::local_server::server::{create_context, server as run_pipe_server};
use crate::run_util::single;
use anyhow::Result;
use chrono::Local;
use log::error;
use spring::plugin::MutableComponentRegistry;
use spring::{auto_config, App};
use spring_web::{WebConfigurator, WebPlugin};
use std::env;
use std::io::Write;

mod api_contract;
mod api_service;
mod context;
mod ctrl_conn;
mod ctrl_data_conn;
mod ctrl_executor;
mod direct_executor;
mod dispatch;
mod http_service;
mod input_command;
mod local_client;
mod local_executor;
mod local_server;
mod pipe;
mod run_util;
mod server_executor;

const LOG_LEVEL: &str = env!("LOG_LEVEL");

#[auto_config(WebConfigurator)]
#[tokio::main]
async fn main() -> Result<()> {
    let lock_path = env::var("REAL_CTRL_HTTP_LOCK_PATH")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "D:/MyTest/Single/real_ctrl_invoker_http_service.lock".to_string());
    let _single_lock = single(lock_path).await;
    env::set_var("RUST_LOG", LOG_LEVEL);
    env_logger::Builder::new()
        .format(|buf, record| {
            writeln!(
                buf,
                "{} [{}] - {}",
                Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                record.level(),
                record.args()
            )
        })
        .parse_default_env()
        .init();

    let context = create_context().await?;
    let pipe_context = context.clone();
    let api = RealCtrlApi::new(context);

    tokio::spawn(async move {
        if let Err(e) = run_pipe_server(&pipe_context).await {
            error!("本地管道服务结束: {}", e);
        }
    });

    App::new()
        .add_component(api)
        .add_plugin(WebPlugin)
        .run()
        .await;
    Ok(())
}
