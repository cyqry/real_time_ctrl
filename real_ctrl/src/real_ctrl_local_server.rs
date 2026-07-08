use crate::local_server::server::start_pipe_server;
use chrono::Local;
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
mod input_command;
mod local_executor;
mod local_server;
mod pipe;
mod server_executor;

const LOG_LEVEL: &str = env!("LOG_LEVEL");

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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

    start_pipe_server().await?;
    Ok(())
}
