use chrono::{DateTime, FixedOffset, Local, Utc};
use log::{Level, LevelFilter};
use std::cmp::Ordering;
use std::{fs, io, path::PathBuf};
use tracing_appender::non_blocking;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_log::AsLog;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{
    fmt::{self, format::Writer, time::FormatTime},
    EnvFilter, Layer, Registry,
};

// 自定义时间格式
#[derive(Clone)]
struct LocalTimer {
    offset: FixedOffset,
}
impl LocalTimer {
    pub fn new() -> Self {
        //北京时间
        let offset = FixedOffset::east_opt(8 * 3600).unwrap();
        Self { offset }
    }
}
impl FormatTime for LocalTimer {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        let utc_now: DateTime<Utc> = Utc::now();
        let beijing_time = utc_now.with_timezone(&self.offset);
        write!(w, "{}", beijing_time.format("%Y-%m-%d %H:%M:%S%.3f"))
    }
}

pub struct LogConfig {
    pub dir: PathBuf,
    pub prefix: String,
    pub file_size_mb: u64,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("./logs"),
            prefix: "app".to_string(),
            file_size_mb: 100, // 每个文件最大100MB
        }
    }
}

pub fn init_logging_with_config(config: LogConfig) -> Result<(), Box<dyn std::error::Error>> {
    // 创建日志目录
    fs::create_dir_all(&config.dir)?;

    //日志的格式
    let log_format = fmt::format()
        .with_ansi(false) // 文件日志禁用 ANSI 颜色
        .with_timer(LocalTimer::new())
        .with_target(false)
        .with_file(true)
        .with_line_number(true)
        .with_thread_ids(false);

    // 创建 ERROR 级别以下的日志轮转 Appender
    let default_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY) // 每天轮转
        .filename_prefix(format!("{}-normal", config.prefix)) // 文件名前缀为 my_app
        .filename_suffix("log")
        .max_log_files(0) // 保留所有日志文件
        .build(&config.dir)?;

    // let (non_blocking_writer, _guard) = non_blocking(default_appender);
    let default_layer = fmt::layer()
        .event_format(log_format.clone())
        .with_writer(default_appender)
        // 过滤器: 结合 EnvFilter 的规则，并使用 filter_fn 明确排除 ERROR 及以上级别
        .with_filter(   tracing_subscriber::filter::filter_fn ( move |metadata| {
            // 排除 ERROR 及以上的事件，避免与 error_layer 重复
            metadata.level().as_log() > Level::Error
        }));

    // 配置文件轮转（按大小）
    let error_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY) // 每天轮转
        // 文件名前缀为 my_app-error
        .filename_prefix(format!("{}-error", config.prefix))
        .filename_suffix("log")
        .max_log_files(0) // 保留所有日志文件
        .build(&config.dir)?;

    let error_layer = fmt::layer()
        .event_format(log_format.clone())
        .with_writer(error_appender)
        .with_filter(tracing_subscriber::filter::LevelFilter::ERROR);

    //  初始化订阅者，同时注册两个日志层
    // SubscriberExt::with() 允许我们附加多个层
    tracing_subscriber::registry()
        .with(default_layer) // 注册非 ERROR 日志层
        .with(error_layer) // 注册 ERROR 日志层
        .init();

    Ok(())
}
