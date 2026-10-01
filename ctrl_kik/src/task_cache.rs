//! 命名任务的磁盘缓存边界：按部署公钥和最终 EXE 文件名确定路径，每次重新验证实际文件。
//!
//! 准备锁跨进程生效，只覆盖检查、收件、原子替换和进程创建。运行期不持有该锁；缓存文件
//! 也不随任务结束删除。随机收件文件始终由独立清理守卫拥有，失败不会破坏旧缓存。

use common::{hidden, task::valid_task_file_name};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom},
    os::windows::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use winapi::um::{
    winbase::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT},
    winnt::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    },
};

/// 每级目录在打开时拒绝既有重解析点，并通过拒绝 DELETE 共享阻止准备期的路径更名。
/// 前提是当前用户的 TEMP/缓存目录可信且不允许其他不受信任用户写入；这里不审计或重设 ACL，
/// 也不承诺抵抗同用户恶意进程、原地重解析属性变更或任意可写共享 TEMP。
type Directories = Arc<Vec<File>>;

/// OS 锁文件不删除，避免两个进程分别锁住已删除旧文件和新建文件而同时进入临界区。
pub struct CacheEntry {
    directories: Directories,
    _lock: Arc<File>,
    path: PathBuf,
}

/// 实际摘要验证使用此句柄。只允许其他进程共享读取，直到 CreateProcess 成功前禁止写入/删除。
/// 它只保护启动时的镜像身份，不把文件名或 SHA-256 当成访问控制或来源认证。
pub struct VerifiedProgram {
    _file: File,
    _directories: Directories,
    path: PathBuf,
}

impl VerifiedProgram {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// 文件句柄必须先释放，再删除本次随机路径；父目录守卫则最后释放。
pub struct IncomingProgram {
    pub file: tokio::fs::File,
    temporary: TemporaryPath,
}

struct TemporaryPath {
    path: PathBuf,
    armed: bool,
    _directories: Directories,
}

impl Drop for TemporaryPath {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        match std::fs::remove_file(&self.path) {
            Ok(()) => return,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(_) => {}
        }
        // 杀毒软件可能短暂占用刚关闭的句柄。仅重试本次随机路径，并继续固定父目录身份。
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let path = self.path.clone();
            let directories = self._directories.clone();
            runtime.spawn(async move {
                let _directories = directories;
                for seconds in [1, 2, 4, 8, 16] {
                    tokio::time::sleep(Duration::from_secs(seconds)).await;
                    match tokio::fs::remove_file(&path).await {
                        Ok(()) => break,
                        Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                        Err(_) => {}
                    }
                }
            });
        }
    }
}

fn opaque_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        for nibble in [byte >> 4, byte & 15] {
            out.push(if nibble < 10 {
                b'0' + nibble
            } else {
                b'a' + nibble - 10
            } as char);
        }
    }
    out
}

fn plain_metadata(file: &File, directory: bool) -> anyhow::Result<()> {
    let metadata = file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err(anyhow::Error::msg(hidden!("任务缓存路径类型不安全")));
    }
    Ok(())
}

fn hold_directory(path: &Path) -> anyhow::Result<File> {
    let file = OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    plain_metadata(&file, true)?;
    Ok(file)
}

fn open_entry(base: &Path, public_key: &str, file_name: &str) -> anyhow::Result<CacheEntry> {
    // 服务端和 Kik 分别校验，不能把远端提供的文件名当作路径。只接收原生 EXE 单文件名，
    // 拒绝设备名、ADS、路径分隔符和 ~，最后一项防止 Windows 自动 8.3 短名绕过同名锁。
    if !valid_task_file_name(file_name) || !base.is_absolute() {
        return Err(anyhow::Error::msg(hidden!("任务缓存文件名或目录无效")));
    }
    // 先逐级检查既有 temp 路径并持有防更名句柄；权限可信是上面声明的运行前提。
    let mut path = PathBuf::new();
    let mut directories = Vec::new();
    for component in base.components() {
        match component {
            Component::Prefix(_) => path.push(component),
            Component::RootDir | Component::Normal(_) => {
                path.push(component);
                directories.push(hold_directory(&path)?);
            }
            _ => return Err(anyhow::Error::msg(hidden!("任务缓存目录无效"))),
        }
    }
    let mut namespace = Sha256::new();
    namespace.update(hidden!("rtc-task-cache-deployment-v1\0").as_bytes());
    namespace.update(public_key.trim().as_bytes());
    // 部署目录直接位于 TEMP 下，省去额外的缓存容器目录；不同部署仍不能共用程序或锁。
    // 不迁移或删除旧目录：其中可能还有运行中的镜像，新路径首次使用时按未命中接收。
    path.push(opaque_hex(&namespace.finalize()));
    match std::fs::create_dir(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    directories.push(hold_directory(&path)?);
    // 锁和程序分目录存放，避免合法程序名与内部锁文件相撞。使用真实文件名让 Windows
    // 处理大小写等价：不要自行 lowercase/Unicode 归一化，后者与文件系统规则不一致。
    let lock_directory = path.join(hidden!(".locks"));
    match std::fs::create_dir(&lock_directory) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    directories.push(hold_directory(&lock_directory)?);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(lock_directory.join(file_name))?;
    plain_metadata(&lock, false)?;
    path.push(file_name);
    Ok(CacheEntry {
        directories: Arc::new(directories),
        _lock: Arc::new(lock),
        path,
    })
}

impl CacheEntry {
    pub async fn acquire(file_name: &str) -> anyhow::Result<Self> {
        Self::acquire_in(
            std::env::temp_dir(),
            common::generated::encrypted_strings::KIK_NOISE_SERVER_PUBLIC_KEY(),
            file_name.to_owned(),
        )
        .await
    }

    async fn acquire_in(
        base: PathBuf,
        public_key: String,
        file_name: String,
    ) -> anyhow::Result<Self> {
        // 若外层取消，阻塞任务的结果一经返回便释放所有句柄，不会留下后台锁拥有者。
        let entry = tokio::task::spawn_blocking(move || open_entry(&base, &public_key, &file_name))
            .await??;
        let deadline =
            tokio::time::Instant::now() + Duration::from_secs(common::task::PREPARE_SECONDS);
        loop {
            match fs4::FileExt::try_lock_exclusive(entry._lock.as_ref()) {
                Ok(()) => return Ok(entry),
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock
                        || error.raw_os_error() == Some(33) => {}
                Err(error) => return Err(error.into()),
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow::Error::msg(hidden!("任务缓存准备忙，请稍后重试")));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// NotFound、长度不同或摘要不同均表示未命中；权限、共享占用和危险路径则明确失败。
    pub async fn inspect(
        &self,
        size: u64,
        hash: [u8; 32],
    ) -> anyhow::Result<Option<VerifiedProgram>> {
        let path = self.path.clone();
        let directories = self.directories.clone();
        let preparation_lock = self._lock.clone();
        let cancel = CancellationToken::new();
        let _cancel_on_drop = cancel.clone().drop_guard();
        tokio::task::spawn_blocking(move || {
            // 外层取消不能提前放开准备锁；阻塞读取看到取消并关闭候选句柄后再允许下一请求。
            let _preparation_lock = preparation_lock;
            let mut file = match OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&path)
            {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            };
            plain_metadata(&file, false)?;
            if file.metadata()?.len() != size {
                return Ok(None);
            }
            let mut digest = Sha256::new();
            let mut buffer = vec![0u8; 1024 * 1024];
            loop {
                if cancel.is_cancelled() {
                    return Err(anyhow::Error::msg(hidden!("任务缓存校验已取消")));
                }
                let bytes = file.read(&mut buffer)?;
                if bytes == 0 {
                    break;
                }
                digest.update(&buffer[..bytes]);
            }
            if digest.finalize().as_slice() != hash {
                return Ok(None);
            }
            Ok(Some(VerifiedProgram {
                _file: file,
                _directories: directories,
                path,
            }))
        })
        .await?
    }

    pub async fn incoming(&self) -> anyhow::Result<IncomingProgram> {
        let mut path = self.path.clone();
        path.set_file_name(hidden!(uuid::Uuid::new_v4(), ".temp"));
        let directories = self.directories.clone();
        tokio::task::spawn_blocking(move || {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .share_mode(FILE_SHARE_READ)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .open(&path)?;
            Ok::<_, anyhow::Error>(IncomingProgram {
                file: tokio::fs::File::from_std(file),
                temporary: TemporaryPath {
                    path,
                    armed: true,
                    _directories: directories,
                },
            })
        })
        .await?
    }

    /// 原子替换发生在相同目录中。Windows 的运行中镜像/共享占用导致替换失败时保留旧文件；
    /// 禁止先删旧文件、直接截断旧文件、终止旧进程，或把旧版本作为本次执行的降级结果。
    pub async fn publish(
        &self,
        incoming: IncomingProgram,
        size: u64,
        hash: [u8; 32],
    ) -> anyhow::Result<VerifiedProgram> {
        self.publish_after(incoming, size, hash, || {}).await
    }

    /// before_commit 仅供模块内测试精确停在提交前；生产传入空操作，不改变提交状态机。
    async fn publish_after(
        &self,
        incoming: IncomingProgram,
        size: u64,
        hash: [u8; 32],
        before_commit: impl FnOnce() + Send + 'static,
    ) -> anyhow::Result<VerifiedProgram> {
        let IncomingProgram { file, temporary } = incoming;
        let file = file.into_std().await;
        let destination = self.path.clone();
        let preparation_lock = self._lock.clone();
        let cancel = CancellationToken::new();
        let _cancel_on_drop = cancel.clone().drop_guard();
        tokio::task::spawn_blocking(move || {
            // Arc 保留同一个已锁句柄，而非新开/复制独立文件锁。外层取消后，旧提交必须完成
            // 或观察到取消才释放，避免迟到 rename 覆盖下一请求的新版本。
            let _preparation_lock = preparation_lock;
            let mut temporary = temporary;
            // 声明顺序保证失败时先关闭文件句柄，再让随机路径清理守卫尝试删除。
            let mut file = file;
            // 收件层负责分片完整性，这里校验当前实际句柄的长度和摘要，避免先覆盖旧缓存
            // 再发现坏文件。提交后 inspect 再验证最终路径并持有只读句柄直到创建进程；
            // 共两遍磁盘摘要，与原链路一致，不额外保留收件层的重复摘要检查。
            plain_metadata(&file, false)?;
            if file.metadata()?.len() != size {
                return Err(anyhow::Error::msg(hidden!("任务缓存提交前长度不匹配")));
            }
            file.seek(SeekFrom::Start(0))?;
            let mut digest = Sha256::new();
            let mut buffer = vec![0u8; 1024 * 1024];
            loop {
                if cancel.is_cancelled() {
                    return Err(anyhow::Error::msg(hidden!("任务缓存提交已取消")));
                }
                let bytes = file.read(&mut buffer)?;
                if bytes == 0 {
                    break;
                }
                digest.update(&buffer[..bytes]);
            }
            if digest.finalize().as_slice() != hash {
                return Err(anyhow::Error::msg(hidden!("任务缓存提交前摘要不匹配")));
            }
            file.sync_all()?;
            drop(file);
            before_commit();
            if cancel.is_cancelled() {
                return Err(anyhow::Error::msg(hidden!("任务缓存提交已取消")));
            }
            std::fs::rename(&temporary.path, &destination).map_err(|error| {
                anyhow::Error::msg(hidden!("任务缓存使用中或无法替换，本次未启动: ", error))
            })?;
            temporary.armed = false;
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        self.inspect(size, hash)
            .await?
            .ok_or_else(|| anyhow::Error::msg(hidden!("任务缓存提交后校验失败，本次未启动")))
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use tokio::io::AsyncWriteExt;

    /// 所有 unit 文件位于仓库 target，测试不修改用户系统临时目录或真实缓存。
    pub struct Root(pub PathBuf);
    impl Root {
        pub fn new() -> Self {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .join("target")
                .join("task-cache-tests")
                .join(uuid::Uuid::new_v4().to_string());
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        pub fn acquire(
            &self,
            file_name: String,
        ) -> impl std::future::Future<Output = anyhow::Result<CacheEntry>> + Send + 'static
        {
            CacheEntry::acquire_in(self.0.clone(), "unit-test-deployment".into(), file_name)
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            let expected = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .join("target")
                .join("task-cache-tests");
            assert!(self.0.starts_with(expected));
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    pub fn file_name(id: u8, extension: &str) -> String {
        format!("named-cache-fixture-{id}.{extension}")
    }
    pub fn hash(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(bytes).into()
    }
    pub async fn publish(entry: &CacheEntry, bytes: &[u8]) -> VerifiedProgram {
        let mut incoming = entry.incoming().await.unwrap();
        incoming.file.write_all(bytes).await.unwrap();
        incoming.file.flush().await.unwrap();
        incoming.file.sync_all().await.unwrap();
        entry
            .publish(incoming, bytes.len() as u64, hash(bytes))
            .await
            .unwrap()
    }
    pub fn child_spec() -> common::task::TaskSpec {
        common::task::TaskSpec {
            asynchronous: false,
            output: true,
            timeout_seconds: 10,
            args: vec![
                "--exact".into(),
                "task_cache::tests::child_fixture".into(),
                "--ignored".into(),
                "--nocapture".into(),
            ],
            default_content: "unused".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{testing::*, *};
    use tokio::{io::AsyncWriteExt, time::timeout};

    #[tokio::test]
    async fn cache_rechecks_actual_bytes_and_preserves_final_file_name() {
        let root = Root::new();
        let name = "原始程序 诊断工具.EXe";
        let entry = root.acquire(name.into()).await.unwrap();
        assert!(entry.inspect(4, hash(b"kC42")).await.unwrap().is_none());
        let program = publish(&entry, b"kC42").await;
        let path = program.path().to_path_buf();
        assert_eq!(path.extension().unwrap(), "EXe");
        assert_eq!(path.file_name().unwrap(), name);
        assert!(path.parent().unwrap().join(".locks").join(name).is_file());
        // TEMP 下只有一层部署命名空间，不能重新引入 rtc-task-cache-v2 容器层。
        let relative = path.strip_prefix(&root.0).unwrap();
        assert_eq!(relative.components().count(), 2);
        let namespace = relative.parent().unwrap().to_str().unwrap();
        assert_eq!(namespace.len(), 64);
        assert!(namespace.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(!root.0.join("rtc-task-cache-v2").exists());
        drop(program);
        assert!(entry.inspect(4, hash(b"kC42")).await.unwrap().is_some());
        assert!(entry.inspect(5, hash(b"kC42")).await.unwrap().is_none());
        std::fs::write(&path, b"DATA").unwrap();
        assert!(entry.inspect(4, hash(b"kC42")).await.unwrap().is_none());
        drop(publish(&entry, b"kC42").await);
        drop(entry);
        let reopened = root.acquire(name.into()).await.unwrap();
        assert_eq!(reopened.path, path);
        assert!(reopened.inspect(4, hash(b"kC42")).await.unwrap().is_some());
        let other = root.acquire(file_name(2, "EXe")).await.unwrap();
        assert_ne!(other.path, path);
        let deployment =
            CacheEntry::acquire_in(root.0.clone(), "other-deployment".into(), name.into())
                .await
                .unwrap();
        assert_ne!(deployment.path, path);
    }

    #[tokio::test]
    async fn windows_equivalent_file_names_share_one_prepare_lock_and_cache() {
        let root = Root::new();
        let owner = root
            .acquire("命名缓存 Case Shared.EXE".into())
            .await
            .unwrap();
        drop(publish(&owner, b"case-alias-cache-regression-payload").await);
        let mut alias = tokio::spawn(root.acquire("命名缓存 case shared.exe".into()));
        assert!(timeout(Duration::from_millis(150), &mut alias)
            .await
            .is_err());
        // 不同最终名不受该准备锁影响，证明不是部署级的全局串行锁。
        let independent = timeout(
            Duration::from_secs(2),
            root.acquire("命名缓存 Independent Image.exe".into()),
        )
        .await
        .unwrap()
        .unwrap();
        drop(independent);
        drop(owner);
        let alias = timeout(Duration::from_secs(2), alias)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(alias
            .inspect(35, hash(b"case-alias-cache-regression-payload"))
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn invalid_incoming_bytes_never_replace_the_previous_named_image() {
        let root = Root::new();
        let entry = root
            .acquire("内容更新验证 固定文件名.exe".into())
            .await
            .unwrap();
        let original = b"named-cache-previous-image-remains-intact";
        drop(publish(&entry, original).await);
        for expected_size in [4, 5] {
            let mut incoming = entry.incoming().await.unwrap();
            let partial_path = incoming.temporary.path.clone();
            incoming.file.write_all(b"BAD!").await.unwrap();
            incoming.file.flush().await.unwrap();
            assert!(entry
                .publish(incoming, expected_size, hash(b"GOOD"))
                .await
                .is_err());
            assert_eq!(std::fs::read(&entry.path).unwrap(), original);
            assert!(!partial_path.exists());
        }
    }

    #[tokio::test]
    async fn verified_handle_blocks_changes_and_parent_rename_until_start() {
        let root = Root::new();
        let entry = root.acquire(file_name(3, "exe")).await.unwrap();
        let program = publish(&entry, b"trusted").await;
        assert!(OpenOptions::new().write(true).open(program.path()).is_err());
        assert!(std::fs::remove_file(program.path()).is_err());
        assert!(std::fs::rename(program.path(), program.path().with_extension("moved")).is_err());
        assert!(std::fs::rename(&root.0, root.0.with_extension("moved")).is_err());
        drop(program);
        std::fs::write(&entry.path, b"changed").unwrap();
        assert!(entry.inspect(7, hash(b"trusted")).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn prepare_lock_cancellation_releases_waiter_without_unlocking_owner() {
        let root = Root::new();
        let owner = root.acquire(file_name(4, "exe")).await.unwrap();
        let mut waiter = tokio::spawn(root.acquire(file_name(4, "exe")));
        assert!(timeout(Duration::from_millis(120), &mut waiter)
            .await
            .is_err());
        waiter.abort();
        assert!(matches!(waiter.await, Err(error) if error.is_cancelled()));
        let mut next = tokio::spawn(root.acquire(file_name(4, "exe")));
        assert!(timeout(Duration::from_millis(120), &mut next)
            .await
            .is_err());
        drop(owner);
        timeout(Duration::from_secs(2), next)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn cancelled_blocking_commit_cannot_overwrite_next_request() {
        let root = Root::new();
        let entry = root.acquire(file_name(45, "exe")).await.unwrap();
        drop(publish(&entry, b"original").await);
        let path = entry.path.clone();
        let mut incoming = entry.incoming().await.unwrap();
        incoming.file.write_all(b"kC42abort").await.unwrap();
        incoming.file.flush().await.unwrap();
        let (at_commit, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let old = tokio::spawn(async move {
            entry
                .publish_after(incoming, 9, hash(b"kC42abort"), move || {
                    at_commit.send(()).unwrap();
                    wait.recv_timeout(Duration::from_secs(5)).unwrap();
                })
                .await
        });
        ready.await.unwrap();
        old.abort();
        assert!(matches!(old.await, Err(error) if error.is_cancelled()));
        let mut next = tokio::spawn(root.acquire(file_name(45, "exe")));
        assert!(
            timeout(Duration::from_millis(120), &mut next)
                .await
                .is_err(),
            "cancelled blocking commit released its lock too early"
        );
        release.send(()).unwrap();
        let next = timeout(Duration::from_secs(2), next)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        drop(publish(&next, b"newest").await);
        assert_eq!(std::fs::read(path).unwrap(), b"newest");
    }

    #[tokio::test]
    async fn cancelled_receive_removes_only_its_random_temp_file() {
        let root = Root::new();
        let entry = root.acquire(file_name(5, "exe")).await.unwrap();
        drop(publish(&entry, b"existing").await);
        let mut incoming = entry.incoming().await.unwrap();
        let incoming_path = incoming.temporary.path.clone();
        incoming.file.write_all(b"unfinished").await.unwrap();
        incoming.file.flush().await.unwrap();
        drop(incoming);
        timeout(Duration::from_secs(3), async {
            while incoming_path.exists() {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(std::fs::read(&entry.path).unwrap(), b"existing");
    }

    #[tokio::test]
    async fn cache_rejects_directory_entries_and_invalid_suffixes() {
        let root = Root::new();
        for extension in ["../exe", "exe:stream", "exe.", "x/y", "a\\b", "x\0"] {
            assert!(root.acquire(file_name(6, extension)).await.is_err());
        }
        // 自动 8.3 短名不能成为第二把锁；设备名、路径或 ADS 也必须在查磁盘前被拒绝。
        for name in [
            "LONGFI~1.EXE",
            "缓存~别名.exe",
            "../escape.exe",
            "CON.exe",
            "CONIN$.exe",
            "named.exe:stream",
            "name.exe ",
            "name.exe.",
        ] {
            assert!(root.acquire(name.into()).await.is_err(), "{name}");
        }
        let longest = format!("{}.exe", "命名边界".repeat(19));
        let longest = root.acquire(longest).await.unwrap();
        drop(publish(&longest, b"unicode-final-name-boundary-regression").await);
        assert!(root
            .acquire(format!("{}.exe", "名".repeat(79)))
            .await
            .is_err());
        let entry = root.acquire(file_name(6, "exe")).await.unwrap();
        std::fs::create_dir(&entry.path).unwrap();
        assert!(entry.inspect(0, hash(b"")).await.is_err());
    }

    /// 只由下面的真实 CreateProcess 测试点名启动；不访问文件系统或网络。
    #[test]
    #[ignore = "child process fixture, invoked explicitly by cache lifecycle tests"]
    fn child_fixture() {
        println!(
            "fixture-image={}",
            std::env::current_exe().unwrap().display()
        );
        std::thread::sleep(Duration::from_secs(3));
    }

    struct Started(Option<tokio::sync::oneshot::Sender<()>>);
    impl Drop for Started {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    #[tokio::test]
    async fn running_image_rejects_update_but_does_not_hold_prepare_lock() {
        let root = Root::new();
        let entry = root.acquire(file_name(7, "exe")).await.unwrap();
        let bytes = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let program = publish(&entry, &bytes).await;
        let path = program.path().to_path_buf();
        let (started, receive) = tokio::sync::oneshot::channel();
        let running = tokio::spawn(async move {
            crate::task_process::run_with_start_guard(
                &path,
                &child_spec(),
                (program, entry, Started(Some(started))),
            )
            .await
        });
        timeout(Duration::from_secs(3), receive)
            .await
            .unwrap()
            .unwrap();
        assert!(
            !running.is_finished(),
            "fixture did not stay alive after native process creation"
        );
        let second = timeout(Duration::from_secs(1), root.acquire(file_name(7, "exe")))
            .await
            .unwrap()
            .unwrap();
        // 第二个任务使用同一最终名和摘要时，旧进程运行期间仍可命中并启动。
        let second_program = second
            .inspect(bytes.len() as u64, hash(&bytes))
            .await
            .unwrap()
            .unwrap();
        let second_path = second_program.path().to_path_buf();
        let (second_started, second_receive) = tokio::sync::oneshot::channel();
        let concurrent = tokio::spawn(async move {
            crate::task_process::run_with_start_guard(
                &second_path,
                &child_spec(),
                (second_program, second, Started(Some(second_started))),
            )
            .await
        });
        timeout(Duration::from_secs(3), second_receive)
            .await
            .unwrap()
            .unwrap();
        assert!(!running.is_finished() && !concurrent.is_finished());
        let updater = timeout(Duration::from_secs(1), root.acquire(file_name(7, "exe")))
            .await
            .unwrap()
            .unwrap();
        let mut incoming = updater.incoming().await.unwrap();
        incoming.file.write_all(b"new bytes").await.unwrap();
        incoming.file.flush().await.unwrap();
        let update = updater.publish(incoming, 9, hash(b"new bytes")).await;
        assert!(update.is_err(), "running native image was replaced");
        assert_eq!(std::fs::read(&updater.path).unwrap(), bytes);
        let result = running.await.unwrap().unwrap();
        assert_eq!(result.code, 0);
        assert_eq!(concurrent.await.unwrap().unwrap().code, 0);
        // 两个进程均已退出，但 Windows 镜像/实时扫描句柄偶尔稍后才释放。生产遇占用直接
        // 失败；测试有界模拟用户稍后再次提交，每次创建新的暂存，不能先删旧镜像来通过。
        timeout(Duration::from_secs(3), async {
            loop {
                let content = b"updated after both named-cache child processes exited";
                let mut incoming = updater.incoming().await.unwrap();
                incoming.file.write_all(content).await.unwrap();
                incoming.file.flush().await.unwrap();
                if let Ok(program) = updater
                    .publish(incoming, content.len() as u64, hash(content))
                    .await
                {
                    drop(program);
                    assert_eq!(std::fs::read(&updater.path).unwrap(), content);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("named cache image remained locked after both children exited");
    }

    #[tokio::test]
    async fn unsupported_suffixes_are_rejected_before_path_lookup_or_native_start() {
        let root = Root::new();
        let bytes = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        for (index, extension) in ["", "bin", "cmd", "BAT", "com"].into_iter().enumerate() {
            assert!(root
                .acquire(file_name(20 + index as u8, extension))
                .await
                .is_err());
            let path = root.0.join("direct-native-test").with_extension(extension);
            std::fs::write(&path, &bytes).unwrap();
            // 旁边的同名 .exe 不是可执行文件；任何隐式补名都会导致测试失败。
            std::fs::write(path.with_extension("exe"), b"must never be selected").unwrap();
            let error = crate::task_process::run_with_start_guard(&path, &child_spec(), ())
                .await
                .err()
                .unwrap();
            assert!(error.to_string().contains("只支持原生 .exe"));
            let mut spec = child_spec();
            spec.asynchronous = true;
            spec.output = false;
            spec.timeout_seconds = 0;
            assert!(crate::task_process::start_async(&path, &spec)
                .unwrap_err()
                .to_string()
                .contains("只支持原生 .exe"));
        }
    }

    #[tokio::test]
    async fn text_script_never_invokes_a_shell() {
        let root = Root::new();
        let entry = root.acquire(file_name(8, "exe")).await.unwrap();
        let program = publish(&entry, b"@echo off\r\nexit /b 0\r\n").await;
        let mut spec = child_spec();
        spec.args.clear();
        let sync_result =
            crate::task_process::run_with_start_guard(program.path(), &spec, ()).await;
        assert!(
            sync_result.is_err(),
            "text EXE unexpectedly created a native process"
        );
        spec.asynchronous = true;
        spec.output = false;
        spec.timeout_seconds = 0;
        assert!(crate::task_process::start_async(program.path(), &spec).is_err());
    }
}
