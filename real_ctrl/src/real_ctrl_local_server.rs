use chrono::Local;
use real_ctrl::local_server::server::start_pipe_server;
use std::env;
use std::io::Write;

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
