use std::io::Write;
use std::env;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;
use chrono::Local;
use log::debug;
use tokio::sync::RwLock;
use common::config::{Config, Id};
use common::generated::encrypted_strings::{PASSWORD, USER_NAME};
use common::host::get_host;
use crate::context::{Agent, Context};
use crate::local_server::server::start_pipe_server;

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

const LOG_LEVEL: &str = env!("LOG_LEVEL");

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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

    start_pipe_server().await?;
    Ok(())
}
