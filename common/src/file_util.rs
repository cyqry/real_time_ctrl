use std::borrow::Cow;
use std::ffi::OsStr;
use std::fs::Metadata;
use std::future::Future;

use async_recursion::async_recursion;
use chrono::{DateTime, Utc};
use chrono_tz::Asia::Shanghai;
use rayon::iter::{ParallelBridge, ParallelIterator};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::SystemTime;
use anyhow::anyhow;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::time::Instant;
use tokio::{fs, io, join};
use walkdir::WalkDir;
use crate::{file_util, time_util};

#[cfg(target_os = "windows")]
pub async fn ls<P: AsRef<Path>>(
    path: P,
    r: bool,
) -> anyhow::Result<Vec<(Option<String>, bool, Option<u64>, Option<String>, Option<String>)>> {
    use std::os::windows::fs::MetadataExt;

    let mut path = path.as_ref().to_path_buf();

    // 如果路径以 ":" 结束（例如 "D:"），则添加反斜杠
    if let Some(os_str) = path.as_os_str().to_str() {
        if os_str.ends_with(":") {
            path.push("\\");
        }
    }
    let mut entries = fs::read_dir(path).await?; //路径不存在在这里返回
    let mut v = vec![];
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let metadata = fs::metadata(&path).await?;
        let file_name = match path.file_name() {
            None => None,
            Some(name) => match name.to_str() {
                None => None,
                Some(n) => Some(n.to_string()),
            },
        };
        let size = match metadata.is_dir() {
            true => {
                match r {
                    true => {
                        Some(get_dir_size(path).await?)
                    }
                    false => {
                        None
                    }
                }
            }
            false => Some(metadata.file_size()),
        };

        v.push((
            file_name,
            metadata.is_file(),
            size,
            metadata
                .created()
                .and_then(|time| Ok(convert_system_time(time)))
                .ok(),
            metadata
                .modified()
                .and_then(|time| Ok(convert_system_time(time)))
                .ok(),
        ));
    }
    Ok(v)
}

pub fn copy_and_rename<P: AsRef<Path>>(original_path: P) -> anyhow::Result<PathBuf> {
    // 确保输入的路径是一个文件
    let original_path = PathBuf::from(original_path.as_ref());
    if !original_path.is_file() {
        return Err(anyhow!( "Provided path is not a file"));
    }

    let mut new_filename = "_".to_owned();
    new_filename.push_str(original_path.file_stem().unwrap().to_str().unwrap());
    new_filename.push_str(".exe");

    let new_path = original_path.with_file_name(new_filename);

    std::fs::copy(&original_path, &new_path)?;
    Ok(new_path)
}

pub async fn read_file<P: AsRef<Path>>(path: P) -> anyhow::Result<Vec<u8>> {
    let file = fs::File::open(path).await?;
    //由于下面预分配了缓冲区，这里貌似不需要BufReader，但还是留着
    let mut reader = BufReader::new(file);
    // 尝试获取文件大小，以预分配缓冲区
    let initial_buffer_size = reader
        .get_ref()
        .metadata()
        .await
        .map(|m| m.len() as usize + 1)
        .unwrap_or(0);
    let mut buffer = Vec::with_capacity(initial_buffer_size);
    reader.read_to_end(&mut buffer).await?;
    Ok(buffer)
}

//返回一个文件数据迭代器
pub async fn read_big_file<P: AsRef<Path>>(path: P, max: usize) {}


pub async fn save_file_with_unique_name<P: AsRef<Path>>(path: P, bys: &[u8]) -> anyhow::Result<()> {
    let mut path = path.as_ref().to_path_buf();
    let original_stem = path.file_stem().and_then(OsStr::to_str).unwrap_or("").to_owned();
    let extension = path.extension().and_then(OsStr::to_str).map(|s| s.to_string());

    let mut counter = 1;
    while path.exists() {
        let mut new_stem = original_stem.clone();
        new_stem.push_str(&format!(" ({})", counter));
        path.set_file_name(new_stem);
        if let Some(ext) = &extension {
            path.set_extension(ext);
        }
        counter += 1;
    }

    save_file(path, bys).await
}

pub async fn save_file<P: AsRef<Path>>(path: P, bys: &[u8]) -> anyhow::Result<()> {
    // 先确保路径中的目录都存在
    if let Some(parent_dir) = path.as_ref().parent() {
        if !parent_dir.exists() {
            fs::create_dir_all(parent_dir).await?;
        }
    }
    let mut file = OpenOptions::new()
        //文件必须可写
        .write(true)
        //文件不存在时创建
        .create(true)
        //写时将原文件弄成0
        .truncate(true)
        .open(path)
        .await?;

    // 如果你知道预期的大小，可以预先分配空间
    file.set_len(bys.len() as u64).await?;

    file.write_all(bys).await?;

    // 确保数据已经物理地写入磁盘
    file.sync_all().await?;

    Ok(())
}

#[tokio::test]
async fn test() {
    use std::time::Duration;
    use time_util::*;
    // let start = Instant::now();
    // //8602103819
    // println!("{}", get_dir_size(r"D:\Myjava").await.unwrap());
    // println!("{:?}", start.elapsed());
    // println!("{:?}", ls("E:", false).await);
    let mut timer = Timer::new();
    // println!("{}", get_dir_size(r"E:\D\").await.unwrap());
    let save_path = "D:/MyTest/test".to_string();
    let mut path = PathBuf::from(save_path.as_str());

    if path.is_dir() {
        path = path.join("1.png");
    };

    println!("{:?}", match save_file_with_unique_name(path.as_path(), &[0, 1]).await {
        Ok(_) => Ok(format!("保存Kik的截屏至:{:?}", path)),
        Err(e) => Err(anyhow!(format!(
                                "保存Kik的截屏至:{:?}失败,err:{}",
                                path, e
                            ))),
    });

    println!("Elapsed time: {} ms", timer.elapsed(TimeUnit::Milliseconds));
}

//这个方法速度快了6,7倍,可以不是异步的，但是由于rust的无栈协程，这里必须是异步的才不会卡着其他的协程
//一个方法不可以等待太久，除非其中占用时间的部分是await的
pub async fn get_dir_size<P: AsRef<Path>>(path: P) -> io::Result<u64> {
    // 先尝试获取目录的元数据，确保其存在并且可以访问
    use walkdir::{DirEntry, WalkDir};
    fs::metadata(path.as_ref()).await?;
    let walk_dir = WalkDir::new(path);
    let handle = tokio::spawn(async move {
        walk_dir
            .into_iter()
            .filter_map(|e| e.ok())
            .par_bridge() // 使用 rayon 的并行迭代器
            .try_fold_with(0u64, |acc, entry: DirEntry| -> io::Result<u64> {
                // 注意 try_fold_with 的使用
                let file_type = entry.file_type();
                if file_type.is_file() {
                    Ok(acc + entry.metadata()?.len())
                } else {
                    Ok(acc)
                }
            })
            .try_reduce(|| 0u64, |a, b| Ok(a + b)) // 这里使用 try_reduce，同时为累加器提供初始化函数
    });
    join!(handle).0?
}

// #[async_recursion(?Send)] //这样这个闭包就非Send
#[async_recursion] //这样标记递归闭包就是Send的
//多线程递归统计大小,较快
pub async fn get_dir_size_b(path: PathBuf) -> io::Result<u64> {
    if path.is_file() {
        return Ok(path.metadata()?.len());
    }

    let mut total_size = 0u64;
    let mut dir = fs::read_dir(path).await?;
    let mut futures = Vec::new();

    while let Some(entry) = dir.next_entry().await? {
        let path = entry.path();
        let file_type = entry.file_type().await?;

        if file_type.is_dir() {
            futures.push(get_dir_size_b(path));
        } else if file_type.is_file() {
            total_size += entry.metadata().await?.len();
        }
    }
    for f in futures {
        match f.await {
            Ok(size) => {
                total_size += size;
            }
            Err(_) => {}
        }
    }
    Ok(total_size)
}

#[async_recursion] //这个宏貌似比直接写Box pin 性能更好
//单线程递归统计大小
pub async fn get_dir_size_t(path: PathBuf) -> io::Result<u64> {
    if path.is_file() {
        return Ok(path.metadata()?.len());
    }

    let mut size = 0;

    let mut dir = fs::read_dir(path).await?;
    while let Some(entry) = dir.next_entry().await? {
        let path = entry.path();

        if path.is_dir() {
            size += get_dir_size_t(path.clone()).await?;
        } else {
            size += path.metadata()?.len();
        }
    }

    Ok(size)
}

fn convert_system_time(time: SystemTime) -> String {
    let datetime: DateTime<Utc> = time.into();

    // 转换为北京时间
    let datetime_beijing = datetime.with_timezone(&Shanghai);

    // 格式化日期和时间
    datetime_beijing.format("%Y-%m-%d %H:%M:%S").to_string()
}
