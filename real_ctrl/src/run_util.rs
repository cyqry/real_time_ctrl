//! 多个 real_ctrl 二进制共用的进程启动工具。
//!
//! 包含遵循 `RUST_LOG` 优先级的日志过滤器，以及通过持有文件句柄维持的单实例锁。

use std::path::Path;
use std::time::Duration;
use tokio::fs::{File, OpenOptions};
use tokio::time;

pub fn apply_log_filter(builder: &mut env_logger::Builder, default_level: &str) {
    if std::env::var_os("RUST_LOG").is_some() {
        builder.parse_default_env();
    } else {
        builder.parse_filters(default_level);
    }
}

pub async fn single<P: AsRef<Path>>(lock_path: P) -> anyhow::Result<File> {
    use fs4::tokio::AsyncFileExt;
    if let Some(parent) = lock_path
        .as_ref()
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .await?;

    let mut last_error = None;
    for _ in 0..3 {
        match lock_file.try_lock_exclusive() {
            Ok(_) => return Ok(lock_file),
            Err(error) => last_error = Some(error),
        }
        time::sleep(Duration::from_secs(1)).await;
    }

    Err(anyhow::anyhow!(
        "已有实例持有运行锁或锁文件不可用: {}",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "未知错误".to_string())
    ))
}
