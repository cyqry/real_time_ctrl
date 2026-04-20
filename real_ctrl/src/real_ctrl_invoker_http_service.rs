#![windows_subsystem = "windows"]


use std::io::Write;
use std::env;
use spring_web::WebConfigurator;
use anyhow::Result;
use chrono::Local;
use log::error;
use spring::{auto_config, App};
use spring_web::WebPlugin;
use crate::local_server::server::start_pipe_server;
use crate::run_util::single;

mod context;
mod ctrl_conn;
mod ctrl_data_conn;
mod ctrl_executor;
mod direct_executor;
mod dispatch;
mod local_executor;
mod server_executor;
mod input_command;
mod pipe;
mod local_server;
mod local_client;
mod http_service;
mod run_util;

const LOG_LEVEL: &str = env!("LOG_LEVEL");

#[auto_config(WebConfigurator)]  // 自动扫描并注册路由
#[tokio::main]
async  fn main() -> Result<()> {
    let f = single("D:/MyTest/Single/real_ctrl_invoker_http_service.lock").await;
    env::set_var("RUST_LOG", LOG_LEVEL);
    env_logger::Builder::new()
        // 关键：定义自定义格式
        .format(|buf, record| {
            writeln!(
                buf,
                "{} [{}] - {}",
                Local::now().format("%Y-%m-%d %H:%M:%S%.3f"), // 添加毫秒
                record.level(),
                record.args()
            )
        })
        .parse_default_env()
        .init();
    tokio::spawn(async move {
        match start_pipe_server().await {
            Ok(_) => {}
            Err(e) => {
                error!("服务结束,err:{}",e)
            }
        }
    });

    //web 服务
    App::new()
        .add_plugin(WebPlugin)  // 注册 Web 插件
        .run()
        .await;
    Ok(())

}








