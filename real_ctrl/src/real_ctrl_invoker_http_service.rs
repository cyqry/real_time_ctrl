use std::io::Write;
use std::env;
use spring_web::WebConfigurator;
use anyhow::Result;
use chrono::Local;
use spring::{auto_config, App};
use spring_web::WebPlugin;

mod pipe;
mod input_command;
mod local_client;
mod http_service;


const LOG_LEVEL: &str = env!("LOG_LEVEL");

#[auto_config(WebConfigurator)]  // 自动扫描并注册路由
#[tokio::main]
async  fn main() -> Result<()> {
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
    App::new()
        .add_plugin(WebPlugin)  // 注册 Web 插件
        .run()
        .await;
    Ok(())

}