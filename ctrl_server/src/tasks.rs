//! 服务端任务目录的按需加载。一个请求只读取一个任务，坏配置不会阻止服务启动或影响其他目录。
//!
//! 根目录始终相对于正在运行的 ctrl_server，不依赖工作目录；缺配置才使用默认值，
//! 读取错误或损坏配置绝不当作默认启用。每次请求重新加载，用户手动替换文件后立即生效。

use anyhow::{bail, Context};
use common::task::{valid_task_file_name, valid_task_name, TaskSpec};
use ctrl_common::task_catalog::{TaskListPage, TaskListRequest};
use serde::Deserialize;
use std::path::{Component, Path, PathBuf};
use tokio::fs;

const CATALOG_SCAN_LIMIT: usize = 4096;
const CATALOG_CHECK_LIMIT: usize = 256;
/// 缺配置时必须判断唯一 EXE，限制目录项总数以免单个大目录拖垮目录查询或执行入口。
const DEFAULT_BINARY_SCAN_LIMIT: usize = 4096;

/// 只读目录页：先有界收集合法直属目录名并排序，再最多检查 256 份配置。
/// 一个任务的损坏/禁用/丢文件只跳过自己；推进的是最后检查名，因此空页也能继续翻页。
pub async fn list(root: &Path, request: &TaskListRequest) -> anyhow::Result<TaskListPage> {
    if !request.valid() {
        bail!("任务目录请求不合法");
    }
    match fs::symlink_metadata(root).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(TaskListPage {
                tasks: vec![],
                next_cursor: None,
            })
        }
        Err(_) => bail!("任务根目录不可读取"),
        Ok(_) => ordinary(root, true).await.context("任务根目录不合法")?,
    }
    let mut directory = fs::read_dir(root).await.context("任务根目录不可读取")?;
    let query = request.query.to_ascii_lowercase();
    let mut candidates = Vec::new();
    let mut count = 0;
    while let Some(entry) = directory.next_entry().await.context("读取任务根目录失败")? {
        count += 1;
        if count > CATALOG_SCAN_LIMIT {
            bail!("任务根目录超过 4096 个目录项，请整理后刷新");
        }
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !valid_task_name(&name)
            || request
                .cursor
                .as_ref()
                .is_some_and(|cursor| &name <= cursor)
            || !name.to_ascii_lowercase().contains(&query)
        {
            continue;
        }
        // 单个条目恰好被手动替换或权限改变时只影响它自己，不能使邻近任务整页不可用。
        if entry
            .file_type()
            .await
            .is_ok_and(|kind| kind.is_dir() && !kind.is_symlink())
        {
            candidates.push(name);
        }
    }
    candidates.sort_unstable();
    let mut tasks = Vec::new();
    for (index, name) in candidates.iter().enumerate().take(CATALOG_CHECK_LIMIT) {
        if let Ok(task) = load(root, name).await {
            // 只做打开与元数据检查，不散列或传输 EXE；真正运行仍重新 load 和完整校验。
            // 先用普通文件校验挡住命名管道等特殊文件，避免 File::open 等待写端。
            if ordinary(&task.binary, false).await.is_ok() {
                if let Ok(file) = fs::File::open(&task.binary).await {
                    if file.metadata().await.is_ok_and(|metadata| {
                        metadata.is_file()
                            && metadata.len() > 0
                            && metadata.len() <= common::file_util::MAX_BIG_FILE_BYTES
                    }) {
                        tasks.push(name.clone());
                    }
                }
            }
        }
        // 满页时不提前消耗下一个有效任务；游标就是当前名，下页严格从它之后开始。
        if tasks.len() == usize::from(request.limit) || index + 1 == CATALOG_CHECK_LIMIT {
            return Ok(TaskListPage {
                tasks,
                next_cursor: (index + 1 < candidates.len()).then(|| name.clone()),
            });
        }
    }
    Ok(TaskListPage {
        tasks,
        next_cursor: None,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    enabled: bool,
    binary: String,
    /// 缺省使用 binary 的文件名；Some("") 是错误配置，不能悄悄退回默认值。
    file_name: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    mode: Mode,
    timeout_seconds: Option<u32>,
    response: Response,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Sync,
    Async,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum ResponseMode {
    Default,
    Output,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    mode: ResponseMode,
    default_content: String,
}

pub struct LoadedTask {
    pub binary: PathBuf,
    pub spec: TaskSpec,
    /// 仅把 Kik 执行需要的最终文件名发出去；源目录、task.toml 与任务名仍留在管理面。
    pub file_name: String,
}

impl LoadedTask {
    fn new(
        binary: PathBuf,
        spec: TaskSpec,
        configured_name: Option<String>,
    ) -> anyhow::Result<Self> {
        let extension = binary
            .extension()
            .map(|ext| ext.to_str().context("任务程序后缀必须为 ASCII 字母数字"))
            .transpose()?
            .unwrap_or_default()
            .to_owned();
        // 当前交付只执行 Windows 原生 EXE；保留后缀原大小写，但不能因改名复用引入
        // 脚本解释器、文件关联或无后缀搜索。Kik 还会独立执行同样的入口校验。
        if !extension.eq_ignore_ascii_case("exe") {
            bail!("任务程序仅支持 .exe 后缀，不支持脚本、其他后缀或无后缀文件");
        }
        let file_name = match configured_name {
            Some(name) => name,
            None => binary
                .file_name()
                .and_then(|name| name.to_str())
                .context("任务源程序文件名必须为 UTF-8")?
                .to_owned(),
        };
        if !valid_task_file_name(&file_name) {
            bail!("任务 file_name 必须为不超过 240 字节的普通 EXE 文件名，不能含路径、设备名或 ~");
        }
        Ok(Self {
            binary,
            spec,
            file_name,
        })
    }
}

pub fn root() -> anyhow::Result<PathBuf> {
    Ok(std::env::current_exe()?
        .parent()
        .context("服务端程序目录不存在")?
        .join("tasks"))
}

/// 逐层拒绝链接和 Windows 重解析点；不能让一个任务借路径访问其他任务目录。
async fn ordinary(path: &Path, directory: bool) -> anyhow::Result<()> {
    let metadata = fs::symlink_metadata(path)
        .await
        .context("任务目录或文件不存在/不可读取")?;
    if metadata.file_type().is_symlink() {
        bail!("任务路径不允许符号链接");
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            bail!("任务路径不允许重解析点");
        }
    }
    if (directory && !metadata.is_dir()) || (!directory && !metadata.is_file()) {
        bail!("任务路径类型错误");
    }
    Ok(())
}

async fn binary_path(directory: &Path, relative: &str) -> anyhow::Result<PathBuf> {
    if relative.is_empty() || relative.len() > 4096 || relative.contains(['\\', ':', '\0']) {
        bail!("binary 必须为任务目录内使用 / 分隔的相对文件路径");
    }
    // `./` 只表示当前任务目录，可在任意层省略；不能使用 canonicalize，
    // 因为它会跟随链接并消去 `..`，让后面的逐层边界检查失去原始路径信息。
    let mut components = Vec::new();
    for component in Path::new(relative).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => components.push(name),
            _ => bail!("binary 不能包含绝对路径或上级目录"),
        }
    }
    // components 会忽略末尾的 `/` 和 `/.`，但它们本意是目录，不能改解释为 EXE。
    if components.is_empty() || matches!(relative.rsplit('/').next(), Some("" | ".")) {
        bail!("binary 必须指向任务目录内的文件，不能只指向目录");
    }
    let mut path = directory.to_path_buf();
    for (index, component) in components.iter().enumerate() {
        // Windows 会归一化尾部空格/点并识别设备名；即使服务端部署在 Linux，也采用统一规则。
        // 例如 ".. " 不能因为词法解析成普通名字，就在 Windows 上变成目录穿越。
        let name = component.to_str().context("binary 路径必须为 UTF-8")?;
        if !ordinary_name(name) {
            bail!("binary 含系统保留名称或不明确的路径片段");
        }
        path.push(component);
        ordinary(&path, index + 1 < components.len()).await?;
    }
    Ok(path)
}

fn ordinary_name(name: &str) -> bool {
    if name.is_empty()
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|ch| ch.is_control() || "<>:\"|?*".contains(ch))
    {
        return false;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    !matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) && !(stem.len() == 4
        && (stem.starts_with("COM") || stem.starts_with("LPT"))
        && matches!(stem.as_bytes()[3], b'1'..=b'9'))
}

pub async fn load(root: &Path, name: &str) -> anyhow::Result<LoadedTask> {
    if !valid_task_name(name) {
        bail!("任务名仅允许 1～64 位字母、数字、下划线和短横线");
    }
    ordinary(root, true).await?;
    let directory = root.join(name);
    ordinary(&directory, true).await?;
    let config_path = directory.join("task.toml");
    let present = match fs::symlink_metadata(&config_path).await {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => bail!("任务配置不可读取"),
    };
    if present {
        ordinary(&config_path, false).await?;
        let bytes = common::file_util::read_file_limited(&config_path, 64 * 1024)
            .await
            .context("任务配置读取失败或超过 64 KiB")?;
        // TOML 错误可能包含参数原文；对外只返回定位类别，不泄漏配置内容。
        let text = std::str::from_utf8(&bytes).context("任务配置必须为 UTF-8")?;
        let config: Config =
            toml::from_str(text).map_err(|_| anyhow::anyhow!("task.toml 格式或字段错误"))?;
        if !config.enabled {
            bail!("任务已禁用");
        }
        let asynchronous = matches!(config.mode, Mode::Async);
        let timeout_seconds = if asynchronous {
            if config.timeout_seconds.is_some() {
                bail!("异步任务不能配置 timeout_seconds");
            }
            0
        } else {
            config
                .timeout_seconds
                .filter(|n| *n > 0)
                .context("同步任务必须配置正整数 timeout_seconds")?
        };
        let spec = TaskSpec {
            asynchronous,
            timeout_seconds,
            output: matches!(config.response.mode, ResponseMode::Output),
            args: config.args,
            default_content: config.response.default_content,
        };
        if !spec.valid() {
            bail!("任务参数/响应超限或异步任务配置了 output");
        }
        return LoadedTask::new(
            binary_path(&directory, &config.binary).await?,
            spec,
            config.file_name,
        );
    }

    let mut entries = fs::read_dir(&directory).await?;
    let mut selected = None;
    let mut count = 0;
    while let Some(entry) = entries.next_entry().await? {
        count += 1;
        if count > DEFAULT_BINARY_SCAN_LIMIT {
            bail!("默认任务目录超过 4096 个目录项，请在 task.toml 中指定 binary");
        }
        let path = entry.path();
        if !path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
        {
            continue;
        }
        // 无配置只选择直属普通文件；目录与链接不参与唯一 EXE 的选择，也不会被跟随读取。
        if !entry.file_type().await?.is_file() {
            continue;
        }
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(ordinary_name)
        {
            bail!("默认任务程序名称不符合普通文件规则");
        }
        ordinary(&path, false).await?;
        if selected.replace(path).is_some() {
            bail!("任务目录有多个 EXE，请在 task.toml 中指定 binary");
        }
    }
    LoadedTask::new(
        selected.context("任务目录没有唯一的 EXE，请上传程序或配置 binary")?,
        TaskSpec {
            asynchronous: false,
            output: true,
            timeout_seconds: 60,
            args: vec![],
            default_content: "执行完成".into(),
        },
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试树只建在仓库 target，Drop 只删除自己创建的 UUID 子目录。
    struct Fixture(PathBuf);
    impl Fixture {
        async fn new() -> Self {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../target/task-config-tests")
                .join(uuid::Uuid::new_v4().to_string());
            fs::create_dir_all(path.join("good")).await.unwrap();
            fs::write(path.join("good/tool.EXE"), b"test-binary")
                .await
                .unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn absent_config_defaults_and_ambiguous_binary_is_rejected() {
        let f = Fixture::new().await;
        let task = load(&f.0, "good").await.unwrap();
        assert!(!task.spec.asynchronous);
        assert!(task.spec.output);
        assert_eq!(task.spec.timeout_seconds, 60);
        assert!(task.spec.args.is_empty());
        assert_eq!(task.file_name, "tool.EXE");
        assert!(!f.0.join("good/task.toml").exists());
        fs::write(f.0.join("good/second.exe"), b"second")
            .await
            .unwrap();
        assert!(load(&f.0, "good").await.is_err());
    }

    #[tokio::test]
    async fn broken_task_does_not_affect_neighbor_and_never_falls_back() {
        let f = Fixture::new().await;
        fs::create_dir(f.0.join("bad")).await.unwrap();
        fs::write(f.0.join("bad/a.exe"), b"test").await.unwrap();
        for content in ["", "broken = [", "enabled = false"] {
            fs::write(f.0.join("bad/task.toml"), content).await.unwrap();
            assert!(load(&f.0, "bad").await.is_err());
            assert!(load(&f.0, "good").await.is_ok());
        }
        for name in ["../good", "good/", "", "a.exe"] {
            assert!(load(&f.0, name).await.is_err());
        }
    }

    #[tokio::test]
    async fn config_updates_disable_paths_and_async_rules() {
        let f = Fixture::new().await;
        let text = "enabled = true\nbinary = 'tool.EXE'\nargs = ['a b', '中文 &']\nmode = 'async'\n[response]\nmode = 'default'\ndefault_content = '已启动'\n";
        let path = f.0.join("good/task.toml");
        fs::write(&path, text).await.unwrap();
        let loaded = load(&f.0, "good").await.unwrap();
        assert!(loaded.spec.asynchronous);
        assert_eq!(loaded.spec.args, ["a b", "中文 &"]);
        for bad in [
            text.replace("enabled = true", "enabled = false"),
            text.replace("'tool.EXE'", "'../good/tool.EXE'"),
            text.replace("mode = 'default'", "mode = 'output'"),
            text.replace("mode = 'async'", "mode = 'async'\ntimeout_seconds = 1"),
        ] {
            fs::write(&path, bad).await.unwrap();
            assert!(load(&f.0, "good").await.is_err());
        }
        fs::remove_file(path).await.unwrap();
        assert!(!load(&f.0, "good").await.unwrap().spec.asynchronous);
    }

    #[tokio::test]
    async fn dot_relative_async_config_preserves_defaults_and_catalog_visibility() {
        let f = Fixture::new().await;
        fs::write(f.0.join("good/scr.exe"), b"async-program")
            .await
            .unwrap();
        // 对应操作员直接上传的异步配置：args、file_name 和 timeout_seconds 均省略。
        let config = "enabled = true\nbinary = './scr.exe'\nmode = 'async'\n[response]\nmode = 'default'\ndefault_content = '任务已启动'\n";
        fs::write(f.0.join("good/task.toml"), config).await.unwrap();
        let loaded = load(&f.0, "good").await.unwrap();
        assert_eq!(loaded.binary, f.0.join("good/scr.exe"));
        assert_eq!(loaded.file_name, "scr.exe");
        assert!(loaded.spec.asynchronous);
        assert!(!loaded.spec.output);
        assert!(loaded.spec.args.is_empty());
        assert_eq!(loaded.spec.timeout_seconds, 0);
        assert_eq!(loaded.spec.default_content, "任务已启动");
        assert_eq!(
            list(&f.0, &catalog_request("", None, 32))
                .await
                .unwrap()
                .tasks,
            ["good"]
        );
    }

    #[tokio::test]
    async fn dot_components_normalize_without_hiding_traversal_or_directory_paths() {
        let f = Fixture::new().await;
        let directory = f.0.join("good");
        fs::create_dir(directory.join("nested")).await.unwrap();
        fs::write(directory.join("nested/tool.exe"), b"nested-program")
            .await
            .unwrap();
        for relative in [
            "./nested/tool.exe",
            "././nested/./tool.exe",
            "nested/././tool.exe",
            "nested//tool.exe",
        ] {
            assert_eq!(
                binary_path(&directory, relative).await.unwrap(),
                directory.join("nested/tool.exe"),
                "{relative:?}"
            );
        }
        // 这些路径即使最终可以落回本任务，也不能先经过上级目录或系统路径解释。
        for relative in [
            "./../good/tool.EXE",
            "nested/../tool.EXE",
            "nested/./../../good/tool.EXE",
            "./nested/.. /tool.exe",
            "/good/tool.EXE",
            "//server/share/tool.exe",
            "C:/tool.exe",
            "C:tool.exe",
            "./nested\\tool.exe",
            "./nested/tool.exe:stream",
            "./NUL.exe",
            "./CON/tool.exe",
            "./nested/tool.exe.",
            "./nested/tool.exe ",
            ".",
            "./",
            "././",
            "./nested",
            "./nested/",
            "./nested/.",
            "./nested/tool.exe/",
            "./nested/tool.exe/.",
        ] {
            assert!(
                binary_path(&directory, relative).await.is_err(),
                "unexpectedly accepted {relative:?}"
            );
        }
        // 越界或坏路径失败不会改变同目录已有合法程序的可用性。
        assert!(binary_path(&directory, "./tool.EXE").await.is_ok());
    }

    #[tokio::test]
    async fn only_direct_regular_executables_participate_in_default_selection() {
        let f = Fixture::new().await;
        fs::create_dir(f.0.join("good/extra.exe")).await.unwrap();
        fs::create_dir(f.0.join("empty")).await.unwrap();
        fs::create_dir(f.0.join("empty/nested")).await.unwrap();
        fs::write(f.0.join("empty/nested/tool.exe"), b"nested")
            .await
            .unwrap();
        assert!(load(&f.0, "good").await.is_ok());
        assert!(load(&f.0, "empty").await.is_err());
        fs::create_dir(f.0.join("good/task.toml")).await.unwrap();
        assert!(load(&f.0, "good").await.is_err());
    }

    #[tokio::test]
    async fn explicit_sync_requires_timeout_and_reloads_current_binary() {
        let f = Fixture::new().await;
        let config = "enabled = true\nbinary = 'tool.EXE'\nmode = 'sync'\ntimeout_seconds = 17\n[response]\nmode = 'output'\ndefault_content = '完成'\n";
        let config_path = f.0.join("good/task.toml");
        fs::write(&config_path, config).await.unwrap();
        let loaded = load(&f.0, "good").await.unwrap();
        assert_eq!(loaded.spec.timeout_seconds, 17);
        fs::write(&loaded.binary, b"updated-by-user").await.unwrap();
        let next = load(&f.0, "good").await.unwrap();
        assert_eq!(loaded.file_name, next.file_name);
        assert_eq!(fs::read(next.binary).await.unwrap(), b"updated-by-user");
        for invalid in [
            config.replace("timeout_seconds = 17\n", ""),
            config.replace("timeout_seconds = 17", "timeout_seconds = 0"),
            config.replace("timeout_seconds = 17", "timeout_seconds = -1"),
            config.replace("timeout_seconds = 17", "timeout_seconds = 4294967296"),
            config.replace("binary = 'tool.EXE'", "binary = '/tool.EXE'"),
            config.replace("binary = 'tool.EXE'", "binary = '../good/tool.EXE'"),
            config.replace("binary = 'tool.EXE'", "binary = '.. /good/tool.EXE'"),
            config.replace("binary = 'tool.EXE'", "binary = 'NUL.exe'"),
            config.replace("binary = 'tool.EXE'", "binary = 'tool.EXE:stream'"),
            config.replace("enabled = true", "enabled = true\nunknown_setting = true"),
        ] {
            fs::write(&config_path, invalid).await.unwrap();
            assert!(load(&f.0, "good").await.is_err());
        }
        fs::write(&config_path, vec![b' '; 64 * 1024 + 1])
            .await
            .unwrap();
        assert!(load(&f.0, "good").await.is_err());
    }

    #[tokio::test]
    async fn configured_file_name_overrides_basename_without_changing_binary_requirements() {
        let f = Fixture::new().await;
        let initial = load(&f.0, "good").await.unwrap();
        let config = "enabled = true\nbinary = 'renamed.EXE'\nmode = 'sync'\ntimeout_seconds = 17\n[response]\nmode = 'output'\ndefault_content = '完成'\n";
        fs::write(f.0.join("good/renamed.EXE"), b"new-bytes")
            .await
            .unwrap();
        fs::write(f.0.join("good/task.toml"), config).await.unwrap();
        let renamed = load(&f.0, "good").await.unwrap();
        assert_eq!(initial.file_name, "tool.EXE");
        assert_eq!(renamed.file_name, "renamed.EXE");
        fs::write(
            f.0.join("good/task.toml"),
            config.replace(
                "mode = 'sync'",
                "file_name = '中文 工具.exe'\nmode = 'sync'",
            ),
        )
        .await
        .unwrap();
        assert_eq!(load(&f.0, "good").await.unwrap().file_name, "中文 工具.exe");
        fs::write(f.0.join("good/without-suffix"), b"native-program")
            .await
            .unwrap();
        fs::write(
            f.0.join("good/task.toml"),
            config.replace("renamed.EXE", "without-suffix"),
        )
        .await
        .unwrap();
        assert!(load(&f.0, "good").await.is_err());
        for suffix in [
            "cmd",
            "CMD",
            "bat",
            "com",
            "bin",
            "ps1",
            "中文",
            "a-b",
            "abcdefghijklmnopq",
        ] {
            let file = format!("renamed.{suffix}");
            fs::write(f.0.join("good").join(&file), b"bytes")
                .await
                .unwrap();
            fs::write(
                f.0.join("good/task.toml"),
                config.replace("renamed.EXE", &file),
            )
            .await
            .unwrap();
            assert!(load(&f.0, "good").await.is_err());
        }
        for suffix in ["exe", "eXe", "EXE"] {
            let file = format!("renamed.{suffix}");
            fs::write(f.0.join("good").join(&file), b"native-program")
                .await
                .unwrap();
            fs::write(
                f.0.join("good/task.toml"),
                config.replace("renamed.EXE", &file),
            )
            .await
            .unwrap();
            let loaded = load(&f.0, "good").await.unwrap();
            assert_eq!(loaded.file_name, file);
        }
    }

    #[tokio::test]
    async fn invalid_file_names_never_fall_back_or_hide_a_healthy_catalog_neighbor() {
        let f = Fixture::new().await;
        fs::create_dir(f.0.join("bad_name")).await.unwrap();
        fs::write(f.0.join("bad_name/source.exe"), b"test-binary")
            .await
            .unwrap();
        for name in [
            "",
            "../escape.exe",
            "COM¹.exe",
            "CON .exe",
            "LONGFI~1.EXE",
            "tool.cmd",
            &format!("{}.exe", "x".repeat(237)),
        ] {
            let config = format!("enabled=true\nbinary='source.exe'\nfile_name='{name}'\nmode='sync'\ntimeout_seconds=1\n[response]\nmode='output'\ndefault_content=''\n");
            fs::write(f.0.join("bad_name/task.toml"), config)
                .await
                .unwrap();
            assert!(load(&f.0, "bad_name").await.is_err());
            assert_eq!(
                list(&f.0, &catalog_request("", None, 32))
                    .await
                    .unwrap()
                    .tasks,
                ["good"]
            );
        }
        // 配置名只决定目的文件名，不能让改了后缀的脚本源文件变成允许执行的 EXE。
        fs::write(f.0.join("bad_name/source.cmd"), b"script")
            .await
            .unwrap();
        fs::write(f.0.join("bad_name/task.toml"), "enabled=true\nbinary='source.cmd'\nfile_name='valid.exe'\nmode='sync'\ntimeout_seconds=1\n[response]\nmode='output'\ndefault_content=''\n").await.unwrap();
        assert!(load(&f.0, "bad_name").await.is_err());
    }

    #[tokio::test]
    async fn nested_binary_uses_leaf_name_and_manual_name_updates_reload_immediately() {
        let f = Fixture::new().await;
        fs::create_dir(f.0.join("good/nested")).await.unwrap();
        fs::write(f.0.join("good/nested/长文件名称.exe"), b"test")
            .await
            .unwrap();
        let config = "enabled=true\nbinary='nested/长文件名称.exe'\nmode='sync'\ntimeout_seconds=1\n[response]\nmode='output'\ndefault_content=''\n";
        fs::write(f.0.join("good/task.toml"), config).await.unwrap();
        assert_eq!(
            load(&f.0, "good").await.unwrap().file_name,
            "长文件名称.exe"
        );
        for name in ["first.exe", "第二 个.EXE"] {
            fs::write(
                f.0.join("good/task.toml"),
                config.replace("mode='sync'", &format!("file_name='{name}'\nmode='sync'")),
            )
            .await
            .unwrap();
            assert_eq!(load(&f.0, "good").await.unwrap().file_name, name);
        }
        fs::remove_file(f.0.join("good/task.toml")).await.unwrap();
        assert_eq!(load(&f.0, "good").await.unwrap().file_name, "tool.EXE");
    }

    #[test]
    fn portable_path_names_do_not_alias_devices_or_parent_directories() {
        for name in [
            ".. ", "a.", "a ", "CON.exe", "nul", "COM1.dat", "LPT9", "conout$", "a?b", "a\nb",
        ] {
            assert!(!ordinary_name(name), "{name:?}");
        }
        for name in ["tool.exe", "中文程序.exe", "v1.0", "COM10.exe", "a b.exe"] {
            assert!(ordinary_name(name), "{name:?}");
        }
    }

    fn catalog_request(query: &str, cursor: Option<String>, limit: u16) -> TaskListRequest {
        TaskListRequest {
            id: "catalog-query".into(),
            target: "owned-kik".into(),
            query: query.into(),
            cursor,
            limit,
        }
    }

    #[tokio::test]
    async fn catalog_paging_filters_bad_neighbors_and_reloads_manual_updates() {
        let f = Fixture::new().await;
        for name in [
            "page_A",
            "page_b",
            "page_C",
            "page_broken",
            "page_empty",
            "page_disabled",
            "page_missing",
        ] {
            fs::create_dir(f.0.join(name)).await.unwrap();
            fs::write(f.0.join(name).join("tool.exe"), b"fixture")
                .await
                .unwrap();
        }
        fs::write(f.0.join("page_broken/task.toml"), "broken=[")
            .await
            .unwrap();
        fs::write(f.0.join("page_empty/tool.exe"), b"")
            .await
            .unwrap();
        fs::write(f.0.join("page_disabled/task.toml"), "enabled=false\nbinary='tool.exe'\nmode='sync'\ntimeout_seconds=1\n[response]\nmode='output'\ndefault_content=''\n").await.unwrap();
        fs::remove_file(f.0.join("page_missing/tool.exe"))
            .await
            .unwrap();
        let first = list(&f.0, &catalog_request("PAGE", None, 2)).await.unwrap();
        assert_eq!(first.tasks, ["page_A", "page_C"]);
        assert_eq!(first.next_cursor.as_deref(), Some("page_C"));
        let second = list(&f.0, &catalog_request("page", first.next_cursor, 2))
            .await
            .unwrap();
        assert_eq!(second.tasks, ["page_b"]);
        assert!(second.next_cursor.is_none());
        // 不缓存可用性结论，操作员移走程序后新的列表立即不再显示它。
        fs::remove_file(f.0.join("page_A/tool.exe")).await.unwrap();
        let refreshed = list(&f.0, &catalog_request("page", None, 50))
            .await
            .unwrap();
        assert_eq!(refreshed.tasks, ["page_C", "page_b"]);
        assert_eq!(
            list(&f.0, &catalog_request("no_match", None, 32))
                .await
                .unwrap()
                .tasks
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn catalog_empty_page_advances_past_256_invalid_tasks() {
        let f = Fixture::new().await;
        for index in 0..256 {
            fs::create_dir(f.0.join(format!("a_bad_{index:03}")))
                .await
                .unwrap();
        }
        let first = list(&f.0, &catalog_request("", None, 32)).await.unwrap();
        assert!(first.tasks.is_empty());
        assert_eq!(first.next_cursor.as_deref(), Some("a_bad_255"));
        let next = list(&f.0, &catalog_request("", first.next_cursor, 32))
            .await
            .unwrap();
        assert_eq!(next.tasks, ["good"]);
        assert!(next.next_cursor.is_none());
    }

    #[tokio::test]
    async fn catalog_large_default_directory_does_not_hide_a_healthy_neighbor() {
        let f = Fixture::new().await;
        let large = f.0.join("a_large");
        fs::create_dir(&large).await.unwrap();
        fs::write(large.join("tool.exe"), b"fixture").await.unwrap();
        for index in 0..DEFAULT_BINARY_SCAN_LIMIT {
            fs::write(large.join(format!("extra_{index}")), b"")
                .await
                .unwrap();
        }
        assert!(load(&f.0, "a_large")
            .await
            .err()
            .unwrap()
            .to_string()
            .contains("4096"));
        assert_eq!(
            list(&f.0, &catalog_request("", None, 32))
                .await
                .unwrap()
                .tasks,
            ["good"]
        );
        // 显式指定程序无需遍历任务子目录，手动补上配置后即恢复可见。
        fs::write(large.join("task.toml"), "enabled=true\nbinary='tool.exe'\nmode='sync'\ntimeout_seconds=1\n[response]\nmode='output'\ndefault_content=''\n").await.unwrap();
        assert_eq!(
            list(&f.0, &catalog_request("", None, 32))
                .await
                .unwrap()
                .tasks,
            ["a_large", "good"]
        );
    }

    #[tokio::test]
    async fn catalog_root_absence_is_empty_but_bad_root_and_scan_overflow_are_errors() {
        let f = Fixture::new().await;
        assert!(
            list(&f.0.join("not_created"), &catalog_request("", None, 32))
                .await
                .unwrap()
                .tasks
                .is_empty()
        );
        assert!(
            list(&f.0.join("good/tool.EXE"), &catalog_request("", None, 32))
                .await
                .is_err()
        );
        for index in 0..CATALOG_SCAN_LIMIT {
            fs::write(f.0.join(format!("entry_{index}")), b"")
                .await
                .unwrap();
        }
        let error = list(&f.0, &catalog_request("does_not_match", None, 1))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("4096"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn catalog_ignores_linked_tasks_and_oversized_sources() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new().await;
        symlink(f.0.join("good"), f.0.join("linked")).unwrap();
        assert!(list(&f.0.join("linked"), &catalog_request("", None, 32))
            .await
            .is_err());
        fs::create_dir(f.0.join("huge")).await.unwrap();
        let file = fs::File::create(f.0.join("huge/tool.exe")).await.unwrap();
        file.set_len(common::file_util::MAX_BIG_FILE_BYTES + 1)
            .await
            .unwrap();
        drop(file);
        assert_eq!(
            list(&f.0, &catalog_request("", None, 32))
                .await
                .unwrap()
                .tasks,
            ["good"]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn links_cannot_load_a_neighbor_program_or_config() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new().await;
        fs::create_dir(f.0.join("linked")).await.unwrap();
        symlink(f.0.join("good/tool.EXE"), f.0.join("linked/tool.exe")).unwrap();
        assert!(load(&f.0, "linked").await.is_err());
        symlink(f.0.join("good"), f.0.join("linked/nested")).unwrap();
        for relative in ["tool.exe", "./tool.exe", "./nested/./tool.EXE"] {
            fs::write(f.0.join("linked/task.toml"), format!("enabled = true\nbinary = '{relative}'\nmode = 'sync'\ntimeout_seconds = 1\n[response]\nmode = 'output'\ndefault_content = ''\n")).await.unwrap();
            assert!(load(&f.0, "linked").await.is_err(), "{relative:?}");
        }
        fs::remove_file(f.0.join("linked/task.toml")).await.unwrap();
        symlink(f.0.join("missing.toml"), f.0.join("linked/task.toml")).unwrap();
        // 悬空配置链接也算存在的错误配置，不能被当作“不存在”而回退默认执行。
        assert!(load(&f.0, "linked").await.is_err());
        assert!(load(&f.0, "good").await.is_ok());
    }
}
