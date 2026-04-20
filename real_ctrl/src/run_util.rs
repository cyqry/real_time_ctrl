use std::path::Path;
use std::time::Duration;
use tokio::fs::{File, OpenOptions};
use tokio::time;

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
