use chrono::Local;
use real_ctrl::dispatch;
use real_ctrl::input_command::InputCommand;
use real_ctrl::local_server::server::create_context;
use real_ctrl::run_util::apply_log_filter;
use std::io::Write;

const LOG_LEVEL: &str = env!("LOG_LEVEL");

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

    let context = create_context().await?;
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
