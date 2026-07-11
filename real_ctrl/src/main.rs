use chrono::Local;
use common::config::{Config, Id, SecurityConfig};
use common::host::get_host_from_env_or_default;
use real_ctrl::context::{Agent, Context};
use real_ctrl::dispatch;
use real_ctrl::input_command::InputCommand;
use real_ctrl::run_util::apply_log_filter;
use std::env;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

const LOG_LEVEL: &str = env!("LOG_LEVEL");
const DEFAULT_SERVER_PORT: &str = env!("REAL_CTRL_DEFAULT_SERVER_PORT");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut logger = env_logger::Builder::new();
    logger.format(|buf, record| {
        writeln!(
            buf,
            "{} [{}] - {}",
            Local::now().format("%Y-%m-%d %H:%M:%S%.3f"), // 添加毫秒
            record.level(),
            record.args()
        )
    });
    apply_log_filter(&mut logger, LOG_LEVEL);
    logger.init();

    let server_port = env::var("REAL_CTRL_SERVER_PORT")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_SERVER_PORT.to_string());

    let agent = Arc::new(RwLock::new(
        Agent::create(&Config {
            id: Id::control_plane_from_env("REAL_CTRL_AUTH_SECRET")?,
            server_host: get_host_from_env_or_default("REAL_CTRL_SERVER_HOST"),
            server_port,
            read_timeout: Duration::from_secs(45),
            write_timeout: Duration::from_secs(45),
            security: SecurityConfig::real_ctrl_from_env(),
        })
        .await?,
    ));

    let context = Context::new(agent);

    context.data_init().await?;
    println!("连接成功");
    loop {
        let mut s = String::new();
        std::io::stdin().read_line(&mut s)?;
        if s.trim().is_empty() {
            continue;
        }
        let i_cmd = match s.trim().parse::<InputCommand>() {
            Ok(command) => command,
            Err(error) => {
                println!("{}", error);
                continue;
            }
        };
        match dispatch::distribution(&context, i_cmd).await {
            Ok(s) => {
                println!("{}", s);
            }
            Err(e) => {
                println!("{}", e);
            }
        }
    }
}
