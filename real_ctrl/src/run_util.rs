use std::path::Path;
use std::time::Duration;
use tokio::fs::{File, OpenOptions};
use tokio::time;

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
