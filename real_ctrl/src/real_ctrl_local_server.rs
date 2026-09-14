//! 仅提供 Windows 命名管道 API 的进程入口。

use chrono::Local;
use real_ctrl::local_server::server::start_pipe_server;
use real_ctrl::run_util::apply_log_filter;
use std::io::Write;

const LOG_LEVEL: &str = env!("LOG_LEVEL");

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut logger = env_logger::Builder::new();
    logger.format(|buf, record| {
        writeln!(
            buf,
            "{} [{}] - {}",
            Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
            record.level(),
            record.args()
        )
    });
    apply_log_filter(&mut logger, LOG_LEVEL);
    logger.init();

    start_pipe_server().await?;
    Ok(())
}
