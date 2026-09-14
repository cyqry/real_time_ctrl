//! 交互式控制台进程入口。
//!
//! 控制台故意逐条读取、执行和打印结果，因此保持串行语义；需要并发调用时应使用 HTTP 或命名管道入口。

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
            // 毫秒有助于定位并发请求和多连接事件的先后顺序。
            Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
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
