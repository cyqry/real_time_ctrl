//! 任务执行扩展的有界线协议。只包含执行说明和临时连接归属证明，不携带服务器路径。
//!
//! 主连接公布一次性密钥，数据连接用独立挑战证明属于该主连接。密钥随主连接换代失效，
//! 不作为账号或机器身份；未命中的任务二进制走既有 FilePart 数据帧，命中时只交换必要摘要与回执。

use crate::{channel::ChannelAttributeKey, hidden, protocol::BufSerializable};
use bytes::{Buf, BufMut, BytesMut};
use hmac::{Hmac, Mac};
use rand::{rngs::OsRng, RngCore};
use sha2::{Digest, Sha256};

pub const MAX_SPEC_BYTES: usize = 32 * 1024;
pub const MAX_ARGS: usize = 128;
pub const MAX_TEXT_BYTES: usize = 8192;
pub const MAX_CACHE_EXTENSION_BYTES: usize = 16;
pub const MAX_TASK_FILE_NAME_BYTES: usize = 240;
pub const PREPARE_SECONDS: u64 = 120;
pub const TRANSFER_SECONDS: u64 = 4 * 60 * 60;
pub const FINISH_SECONDS: u64 = 60;
/// 控制端必须给服务端配置加载留出回包余量，不能与服务端的准备期限同时到期。
pub const BUDGET_ACK_SECONDS: u64 = PREPARE_SECONDS + FINISH_SECONDS;
pub const MAX_WAIT_SECONDS: u64 =
    u32::MAX as u64 + 4 * PREPARE_SECONDS + TRANSFER_SECONDS + 3 * FINISH_SECONDS;
/// 数据连接已经证明的主连接密钥摘要；不保存明文密钥到连接属性。
pub const TASK_DATA_BINDING: ChannelAttributeKey<[u8; 32]> =
    ChannelAttributeKey::new(0x7461_736b_6269_6e64);
/// 只属于当前主连接的短期密钥；断线后随 Channel 销毁，不落盘。
pub const TASK_MAIN_KEY: ChannelAttributeKey<[u8; 32]> =
    ChannelAttributeKey::new(0x7461_736b_6d61_696e);
/// 当前主连接明确协商过缓存协议；只属于连接代次，不能沿用上一次重连的能力。
pub const TASK_CACHE_CAPABLE: ChannelAttributeKey<bool> =
    ChannelAttributeKey::new(0x7461_736b_6361_6368);
/// 当前主连接协商了显式文件名缓存；旧缓存能力不能代替它，重连时必须重新声明。
pub const TASK_NAMED_CACHE_CAPABLE: ChannelAttributeKey<bool> =
    ChannelAttributeKey::new(0x7461_736b_6e61_6d65);

/// 旧协议的不透明文件标识；只保留旧帧识别，不再作为新任务文件名。
/// extension 不含点；允许无后缀，非空时只接受短 ASCII 字母数字，阻止路径与流名称注入。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskCacheKey {
    pub id: [u8; 32],
    pub extension: String,
}

impl TaskCacheKey {
    pub fn valid(&self) -> bool {
        self.extension.len() <= MAX_CACHE_EXTENSION_BYTES
            && self
                .extension
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric())
    }
}

/// 旧协议的固定标识算法，只保留用于兼容性测试；新任务路径使用最终 file_name。
/// 此标识不可逆，但不是秘密；低熵任务名仍可能被离线枚举猜测。
pub fn cache_id(name: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(hidden!("rtc-task-cache-name-v1\0").as_bytes());
    digest.update(name.as_bytes());
    digest.finalize().into()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskSpec {
    pub asynchronous: bool,
    pub output: bool,
    pub timeout_seconds: u32,
    pub args: Vec<String>,
    pub default_content: String,
}

impl TaskSpec {
    pub fn valid(&self) -> bool {
        (!self.asynchronous || (!self.output && self.timeout_seconds == 0))
            && (self.asynchronous || self.timeout_seconds > 0)
            && self.args.len() <= MAX_ARGS
            && self
                .args
                .iter()
                .all(|s| s.len() <= MAX_TEXT_BYTES && !s.contains('\0'))
            && self.args.iter().map(String::len).sum::<usize>() <= 16 * 1024
            && self.default_content.len() <= MAX_TEXT_BYTES
            && !self.default_content.contains('\0')
    }

    pub fn wait_seconds(&self) -> u64 {
        // 从预算通知开始：槽位 P、源摘要 P、缓存/接收准备 P+F、传输，以及执行 T+P+F。
        // 本式再额外保留 F，避免服务端正收尾时控制端同时超时；它不负责杀远端进程。
        4 * PREPARE_SECONDS
            + TRANSFER_SECONDS
            + u64::from(self.timeout_seconds)
            + 3 * FINISH_SECONDS
    }
}

/// 任务名只是目录名，不能成为任意文件路径或 shell 片段。
pub fn valid_task_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// 服务端和 Kik 共用的 Windows 原生 EXE 文件名规则，与双方运行所在系统无关。
/// 只允许单个文件名，禁止路径、ADS 和系统设备名；UTF-8 上限同时给临时提交后缀留余量。
pub fn valid_task_file_name(name: &str) -> bool {
    if name.is_empty()
        || name.len() > MAX_TASK_FILE_NAME_BYTES
        || name.ends_with(['.', ' '])
        || name.chars().any(|ch| {
            ch.is_control()
                || matches!(
                    ch,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' | '~'
                )
        })
    {
        return false;
    }
    let Some((base, extension)) = name.rsplit_once('.') else {
        return false;
    };
    if base.is_empty() || !extension.eq_ignore_ascii_case(&hidden!("exe")) {
        return false;
    }
    // Windows 在扩展名之前也识别设备名；连 CON.foo.exe、COM¹.exe 都不能成为普通程序。
    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ');
    if hidden!("CON|PRN|AUX|NUL|CONIN$|CONOUT$")
        .split('|')
        .any(|device| stem.eq_ignore_ascii_case(device))
    {
        return false;
    }
    for prefix in [hidden!("COM"), hidden!("LPT")] {
        if stem.len() >= prefix.len()
            && stem.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
        {
            let mut tail = stem[prefix.len()..].chars();
            if tail
                .next()
                .is_some_and(|ch| matches!(ch, '1'..='9' | '¹' | '²' | '³'))
                && tail.next().is_none()
            {
                return false;
            }
        }
    }
    true
}

pub fn random_key() -> [u8; 32] {
    let mut key = [0; 32];
    OsRng.fill_bytes(&mut key);
    key
}

pub fn binding_id(key: &[u8; 32]) -> [u8; 32] {
    Sha256::digest(key).into()
}

pub fn binding_proof(key: &[u8; 32], challenge: &[u8; 32]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).unwrap_or_else(|_| unreachable!());
    mac.update(hidden!("rtc-task-data-v1").as_bytes());
    mac.update(challenge);
    mac.finalize().into_bytes().into()
}

pub fn verify_binding(key: &[u8; 32], challenge: &[u8; 32], proof: &[u8; 32]) -> bool {
    let Ok(mut mac) = <Hmac<Sha256> as Mac>::new_from_slice(key) else {
        return false;
    };
    mac.update(hidden!("rtc-task-data-v1").as_bytes());
    mac.update(challenge);
    mac.verify_slice(proof).is_ok()
}

/// 不派生 Debug：Hello/Proof 中包含短期认证材料，不能进入诊断日志。
#[derive(Clone)]
pub enum TaskFrame {
    Hello([u8; 32]),
    HelloAck,
    BindRequest,
    Challenge([u8; 32]),
    Proof([u8; 32]),
    Bound,
    Prepare {
        id: String,
        size: u64,
        hash: [u8; 32],
        spec: TaskSpec,
    },
    Ready(String),
    /// 服务端已放弃传输；只清理仍在接收的临时文件，不能终止已经启动的程序。
    AbandonTransfer(String),
    /// 9～12 是显式缓存能力扩展；0～8 的旧布局保持不变。
    HelloCached([u8; 32]),
    HelloCachedAck,
    PrepareCached {
        id: String,
        size: u64,
        hash: [u8; 32],
        spec: TaskSpec,
        cache: TaskCacheKey,
    },
    /// 表示本地完整摘要已匹配，服务端禁止再发分片；执行结果仍使用独立的 RespExtra。
    CacheHit(String),
    /// 13～15 是显式文件名协议；接收方必须完成本代主连接握手后才执行 PrepareNamed。
    HelloNamed([u8; 32]),
    HelloNamedAck,
    PrepareNamed {
        id: String,
        size: u64,
        hash: [u8; 32],
        spec: TaskSpec,
        file_name: String,
    },
}

impl std::fmt::Debug for TaskFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&hidden!("TaskFrame(redacted)"))
    }
}

fn put_text(buf: &mut BytesMut, text: &str) {
    buf.put_u32(text.len() as u32);
    buf.put_slice(text.as_bytes());
}

fn take_text(buf: &mut BytesMut, max: usize) -> Option<String> {
    if buf.len() < 4 {
        return None;
    }
    let n = buf.get_u32() as usize;
    if n > max || n > buf.len() {
        return None;
    }
    let text = String::from_utf8(buf.split_to(n).to_vec()).ok()?;
    (!text.contains('\0')).then_some(text)
}

impl BufSerializable for TaskFrame {
    fn to_buf(&self) -> BytesMut {
        let mut b = BytesMut::new();
        match self {
            Self::Hello(key) | Self::HelloCached(key) | Self::HelloNamed(key) => {
                b.put_u8(match self {
                    Self::Hello(_) => 0,
                    Self::HelloCached(_) => 9,
                    _ => 13,
                });
                b.put_slice(key);
            }
            Self::HelloAck => b.put_u8(1),
            Self::HelloCachedAck => b.put_u8(10),
            Self::HelloNamedAck => b.put_u8(14),
            Self::BindRequest => b.put_u8(2),
            Self::Challenge(value) => {
                b.put_u8(3);
                b.put_slice(value);
            }
            Self::Proof(value) => {
                b.put_u8(4);
                b.put_slice(value);
            }
            Self::Bound => b.put_u8(5),
            Self::Prepare {
                id,
                size,
                hash,
                spec,
            }
            | Self::PrepareCached {
                id,
                size,
                hash,
                spec,
                ..
            }
            | Self::PrepareNamed {
                id,
                size,
                hash,
                spec,
                ..
            } => {
                b.put_u8(match self {
                    Self::Prepare { .. } => 6,
                    Self::PrepareCached { .. } => 11,
                    _ => 15,
                });
                put_text(&mut b, id);
                b.put_u64(*size);
                b.put_slice(hash);
                b.put_u8(u8::from(spec.asynchronous));
                b.put_u8(u8::from(spec.output));
                b.put_u32(spec.timeout_seconds);
                b.put_u32(spec.args.len() as u32);
                for arg in &spec.args {
                    put_text(&mut b, arg);
                }
                put_text(&mut b, &spec.default_content);
                if let Self::PrepareCached { cache, .. } = self {
                    b.put_slice(&cache.id);
                    put_text(&mut b, &cache.extension);
                }
                if let Self::PrepareNamed { file_name, .. } = self {
                    put_text(&mut b, file_name);
                }
            }
            Self::Ready(id) => {
                b.put_u8(7);
                put_text(&mut b, id);
            }
            Self::AbandonTransfer(id) => {
                b.put_u8(8);
                put_text(&mut b, id);
            }
            Self::CacheHit(id) => {
                b.put_u8(12);
                put_text(&mut b, id);
            }
        }
        b
    }

    fn from_buf(mut b: BytesMut) -> Option<Self> {
        if b.is_empty() || b.len() > MAX_SPEC_BYTES {
            return None;
        }
        let code = b.get_u8();
        let result = match code {
            0 | 3 | 4 | 9 | 13 if b.len() == 32 => {
                let value: [u8; 32] = b.split_to(32).as_ref().try_into().ok()?;
                match code {
                    0 => Self::Hello(value),
                    3 => Self::Challenge(value),
                    4 => Self::Proof(value),
                    9 => Self::HelloCached(value),
                    _ => Self::HelloNamed(value),
                }
            }
            1 => Self::HelloAck,
            10 => Self::HelloCachedAck,
            14 => Self::HelloNamedAck,
            2 => Self::BindRequest,
            5 => Self::Bound,
            6 | 11 | 15 => {
                let id = take_text(&mut b, 128)?;
                if id.trim().is_empty() || id.chars().any(char::is_control) || b.len() < 50 {
                    return None;
                }
                let size = b.get_u64();
                if size == 0 || size > crate::file_util::MAX_BIG_FILE_BYTES {
                    return None;
                }
                let hash = b.split_to(32).as_ref().try_into().ok()?;
                let asynchronous = match b.get_u8() {
                    0 => false,
                    1 => true,
                    _ => return None,
                };
                let output = match b.get_u8() {
                    0 => false,
                    1 => true,
                    _ => return None,
                };
                let timeout_seconds = b.get_u32();
                let count = b.get_u32() as usize;
                if count > MAX_ARGS {
                    return None;
                }
                let mut args = Vec::with_capacity(count);
                for _ in 0..count {
                    args.push(take_text(&mut b, MAX_TEXT_BYTES)?);
                }
                let spec = TaskSpec {
                    asynchronous,
                    output,
                    timeout_seconds,
                    args,
                    default_content: take_text(&mut b, MAX_TEXT_BYTES)?,
                };
                if !spec.valid() {
                    return None;
                }
                if code == 6 {
                    Self::Prepare {
                        id,
                        size,
                        hash,
                        spec,
                    }
                } else if code == 11 {
                    if b.len() < 36 {
                        return None;
                    }
                    let cache = TaskCacheKey {
                        id: b.split_to(32).as_ref().try_into().ok()?,
                        extension: take_text(&mut b, MAX_CACHE_EXTENSION_BYTES)?,
                    };
                    if !cache.valid() {
                        return None;
                    }
                    Self::PrepareCached {
                        id,
                        size,
                        hash,
                        spec,
                        cache,
                    }
                } else {
                    let file_name = take_text(&mut b, MAX_TASK_FILE_NAME_BYTES)?;
                    if !valid_task_file_name(&file_name) {
                        return None;
                    }
                    Self::PrepareNamed {
                        id,
                        size,
                        hash,
                        spec,
                        file_name,
                    }
                }
            }
            7 | 8 | 12 => {
                let id = take_text(&mut b, 128)?;
                if id.trim().is_empty() || id.chars().any(char::is_control) {
                    return None;
                }
                match code {
                    7 => Self::Ready(id),
                    8 => Self::AbandonTransfer(id),
                    _ => Self::CacheHit(id),
                }
            }
            _ => return None,
        };
        b.is_empty().then_some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_spec_round_trip_and_every_truncation_rejected() {
        let spec = TaskSpec {
            asynchronous: false,
            output: true,
            timeout_seconds: 60,
            args: vec!["含空格 参数 &".into()],
            default_content: "完成".into(),
        };
        let encoded = TaskFrame::Prepare {
            id: "run-1".into(),
            size: 100,
            hash: [3; 32],
            spec: spec.clone(),
        }
        .to_buf();
        match TaskFrame::from_buf(encoded.clone()).unwrap() {
            TaskFrame::Prepare { spec: decoded, .. } => assert_eq!(spec, decoded),
            _ => panic!("wrong variant"),
        }
        for len in 0..encoded.len() {
            assert!(TaskFrame::from_buf(BytesMut::from(&encoded[..len])).is_none());
        }
        let mut trailing = encoded;
        trailing.put_u8(0);
        assert!(TaskFrame::from_buf(trailing).is_none());
    }

    #[test]
    fn binding_challenge_and_session_must_match() {
        let key = random_key();
        let nonce = random_key();
        let proof = binding_proof(&key, &nonce);
        assert!(verify_binding(&key, &nonce, &proof));
        assert!(!verify_binding(&random_key(), &nonce, &proof));
        assert!(!verify_binding(&key, &random_key(), &proof));
    }

    #[test]
    fn names_and_async_options_are_strict() {
        for name in ["", "..", "a/b", "a.exe", "a b", "a\0"] {
            assert!(!valid_task_name(name));
        }
        assert!(valid_task_name("task_01-A"));
        let mut spec = TaskSpec {
            asynchronous: true,
            output: false,
            timeout_seconds: 0,
            args: vec![],
            default_content: String::new(),
        };
        assert!(spec.valid());
        spec.output = true;
        assert!(!spec.valid());
    }

    #[test]
    fn binding_and_abandon_frames_reject_truncation_and_trailing_bytes() {
        for frame in [
            TaskFrame::Hello([1; 32]),
            TaskFrame::HelloAck,
            TaskFrame::BindRequest,
            TaskFrame::Challenge([2; 32]),
            TaskFrame::Proof([3; 32]),
            TaskFrame::Bound,
            TaskFrame::Ready("request-1".into()),
            TaskFrame::AbandonTransfer("request-1".into()),
            TaskFrame::HelloCached([4; 32]),
            TaskFrame::HelloCachedAck,
            TaskFrame::HelloNamed([5; 32]),
            TaskFrame::HelloNamedAck,
            TaskFrame::CacheHit("request-1".into()),
        ] {
            let encoded = frame.to_buf();
            assert_eq!(
                TaskFrame::from_buf(encoded.clone()).unwrap().to_buf(),
                encoded
            );
            for len in 0..encoded.len() {
                assert!(TaskFrame::from_buf(BytesMut::from(&encoded[..len])).is_none());
            }
            let mut extra = encoded;
            extra.put_u8(0);
            assert!(TaskFrame::from_buf(extra).is_none());
        }
        for id in ["", " ", "\n", "valid\0suffix"] {
            assert!(TaskFrame::from_buf(TaskFrame::AbandonTransfer(id.into()).to_buf()).is_none());
        }
        assert!(
            TaskFrame::from_buf(TaskFrame::AbandonTransfer("a".repeat(129)).to_buf()).is_none()
        );
        assert!(TaskFrame::from_buf(BytesMut::from(&[255][..])).is_none());
    }

    #[test]
    fn cached_prepare_preserves_legacy_layout_and_rejects_malformed_cache_keys() {
        let spec = TaskSpec {
            asynchronous: false,
            output: true,
            timeout_seconds: 60,
            args: vec!["参数 空格".into()],
            default_content: "完成".into(),
        };
        let legacy = TaskFrame::Prepare {
            id: "request".into(),
            size: 17,
            hash: [2; 32],
            spec: spec.clone(),
        }
        .to_buf();
        assert_eq!(legacy[0], 6);
        for extension in ["", "EXE", "a1", "a1b2c3d4e5f6g7h8"] {
            let cache = TaskCacheKey {
                id: [9; 32],
                extension: extension.into(),
            };
            let encoded = TaskFrame::PrepareCached {
                id: "request".into(),
                size: 17,
                hash: [2; 32],
                spec: spec.clone(),
                cache: cache.clone(),
            }
            .to_buf();
            assert_eq!(encoded[0], 11);
            assert_eq!(&encoded[1..legacy.len()], &legacy[1..]);
            match TaskFrame::from_buf(encoded.clone()).unwrap() {
                TaskFrame::PrepareCached { cache: decoded, .. } => assert_eq!(decoded, cache),
                _ => panic!("wrong variant"),
            }
            for len in 0..encoded.len() {
                assert!(TaskFrame::from_buf(BytesMut::from(&encoded[..len])).is_none());
            }
            let mut extra = encoded;
            extra.put_u8(0);
            assert!(TaskFrame::from_buf(extra).is_none());
        }
        for extension in [
            ".exe",
            "../exe",
            "a/b",
            "a\\b",
            "exe:stream",
            "空",
            "a b",
            "a\0b",
            "a1b2c3d4e5f6g7h89",
        ] {
            let cache = TaskCacheKey {
                id: [9; 32],
                extension: extension.into(),
            };
            assert!(!cache.valid());
            assert!(TaskFrame::from_buf(
                TaskFrame::PrepareCached {
                    id: "request".into(),
                    size: 17,
                    hash: [2; 32],
                    spec: spec.clone(),
                    cache,
                }
                .to_buf()
            )
            .is_none());
        }
        for id in ["", " ", "\n", "valid\0suffix"] {
            assert!(TaskFrame::from_buf(TaskFrame::CacheHit(id.into()).to_buf()).is_none());
        }
        assert!(TaskFrame::from_buf(TaskFrame::CacheHit("a".repeat(129)).to_buf()).is_none());
    }

    #[test]
    fn cache_name_is_stable_distinct_and_domain_separated() {
        assert_eq!(cache_id("task-a"), cache_id("task-a"));
        assert_ne!(cache_id("task-a"), cache_id("task-b"));
        assert_ne!(cache_id("task-a"), cache_id("Task-a"));
        assert_ne!(
            cache_id("task-a"),
            <[u8; 32]>::from(Sha256::digest(b"task-a"))
        );
    }

    #[test]
    fn named_file_validation_is_portable_and_bounded() {
        for name in [
            "tool.exe",
            "my tool.EXE",
            "中文 工具.exe",
            "COM10.exe",
            "长文件名称.exe",
            "x.y.exe",
            "工具😀.exe",
        ] {
            assert!(valid_task_file_name(name), "{name}");
        }
        for name in [
            "",
            ".exe",
            "..",
            "tool",
            "tool.cmd",
            "tool.exe ",
            "tool.exe.",
            "/tool.exe",
            "../tool.exe",
            "a/tool.exe",
            "a\\tool.exe",
            "C:tool.exe",
            "tool.exe:stream",
            "t<ool.exe",
            "t>ool.exe",
            "t|ool.exe",
            "t?ool.exe",
            "t*ool.exe",
            "t\"ool.exe",
            "tool\0.exe",
            "tool\n.exe",
            "CON.exe",
            "con.foo.exe",
            "CON .exe",
            "PRN.exe",
            "AUX.exe",
            "NUL.exe",
            "COM1.exe",
            "LPT9.exe",
            "COM¹.exe",
            "COM².exe",
            "COM³.exe",
            "LPT¹.exe",
            "LPT².exe",
            "LPT³.exe",
            "COM¹ .exe",
            "CONIN$.exe",
            "CONOUT$.exe",
            "LONGFI~1.EXE",
            "legit~name.exe",
        ] {
            assert!(!valid_task_file_name(name), "{name:?}");
        }
        assert!(valid_task_file_name(&format!("{}.exe", "x".repeat(236))));
        assert!(!valid_task_file_name(&format!("{}.exe", "x".repeat(237))));
        assert!(valid_task_file_name(&format!("{}.exe", "中".repeat(78))));
        assert!(!valid_task_file_name(&format!("{}.exe", "中".repeat(79))));
    }

    #[test]
    fn named_prepare_round_trips_and_rejects_malformed_names_without_legacy_fallback() {
        let frame = |file_name: &str| TaskFrame::PrepareNamed {
            id: "run-1".into(),
            size: 17,
            hash: [5; 32],
            spec: TaskSpec {
                asynchronous: false,
                output: true,
                timeout_seconds: 60,
                args: vec!["a b".into()],
                default_content: "完成".into(),
            },
            file_name: file_name.into(),
        };
        let name = "中文 工具.EXE";
        let encoded = frame(name).to_buf();
        assert_eq!(encoded[0], 15);
        assert!(matches!(TaskFrame::from_buf(encoded.clone()),
            Some(TaskFrame::PrepareNamed { file_name, .. }) if file_name == name));
        for len in 0..encoded.len() {
            assert!(TaskFrame::from_buf(BytesMut::from(&encoded[..len])).is_none());
        }
        let mut trailing = encoded.clone();
        trailing.put_u8(0);
        assert!(TaskFrame::from_buf(trailing).is_none());
        let mut invalid_utf8 = encoded.clone();
        *invalid_utf8.last_mut().unwrap() = 255;
        assert!(TaskFrame::from_buf(invalid_utf8).is_none());
        for name in [
            "",
            "../tool.exe",
            "COM¹.exe",
            "tool.cmd",
            "LONGFI~1.EXE",
            &"x".repeat(241),
        ] {
            assert!(TaskFrame::from_buf(frame(name).to_buf()).is_none());
        }
        // 扩展名字段和文件名字段不能互相冒用；仅换帧码不会通过新旧解析。
        let mut old_code = encoded;
        old_code[0] = 11;
        assert!(TaskFrame::from_buf(old_code).is_none());
        assert_eq!(TaskFrame::HelloNamed([0; 32]).to_buf()[0], 13);
        assert_eq!(TaskFrame::HelloNamedAck.to_buf()[0], 14);
    }

    #[test]
    fn invalid_execution_specs_and_file_lengths_never_decode() {
        let valid = TaskSpec {
            asynchronous: false,
            output: true,
            timeout_seconds: 60,
            args: vec![],
            default_content: "done".into(),
        };
        let mut invalid = Vec::new();
        let mut spec = valid.clone();
        spec.timeout_seconds = 0;
        invalid.push(spec);
        let mut spec = valid.clone();
        spec.asynchronous = true;
        invalid.push(spec);
        let mut spec = valid.clone();
        spec.args = vec![String::new(); MAX_ARGS + 1];
        invalid.push(spec);
        let mut spec = valid.clone();
        spec.args = vec!["a".repeat(MAX_TEXT_BYTES + 1)];
        invalid.push(spec);
        let mut spec = valid.clone();
        spec.args = vec!["a".repeat(MAX_TEXT_BYTES); 3];
        invalid.push(spec);
        let mut spec = valid.clone();
        spec.args = vec!["a\0b".into()];
        invalid.push(spec);
        let mut spec = valid.clone();
        spec.default_content = "a".repeat(MAX_TEXT_BYTES + 1);
        invalid.push(spec);
        for spec in invalid {
            assert!(!spec.valid());
            let encoded = TaskFrame::Prepare {
                id: "r".into(),
                size: 1,
                hash: [0; 32],
                spec,
            }
            .to_buf();
            assert!(TaskFrame::from_buf(encoded).is_none());
        }
        for size in [0, crate::file_util::MAX_BIG_FILE_BYTES + 1, u64::MAX] {
            let encoded = TaskFrame::Prepare {
                id: "r".into(),
                size,
                hash: [0; 32],
                spec: valid.clone(),
            }
            .to_buf();
            assert!(TaskFrame::from_buf(encoded).is_none());
        }
        let mut invalid_bool = TaskFrame::Prepare {
            id: "r".into(),
            size: 1,
            hash: [0; 32],
            spec: valid,
        }
        .to_buf();
        invalid_bool[1 + 4 + 1 + 8 + 32] = 2;
        assert!(TaskFrame::from_buf(invalid_bool).is_none());
    }

    #[test]
    fn largest_execution_budget_has_room_for_all_outer_deadlines() {
        let spec = TaskSpec {
            asynchronous: false,
            output: false,
            timeout_seconds: u32::MAX,
            args: vec![],
            default_content: String::new(),
        };
        assert!(spec.valid());
        assert_eq!(spec.wait_seconds(), MAX_WAIT_SECONDS);
        assert!(std::time::Instant::now()
            .checked_add(std::time::Duration::from_secs(MAX_WAIT_SECONDS))
            .is_some());
    }
}
