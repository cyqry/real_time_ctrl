//! Linux 中继服务的进程入口。
//!
//! 这里仅负责合并构建默认值和运行环境变量、初始化日志与账号策略，然后把两个监听端口交给
//! `core::server`。连接认证、会话和数据路由都不应堆回 `main`。

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use common::config::{Config, Id, SecurityConfig};
use common::hidden;
use core::account::AccountRegistry;
use core::context::Context;
use core::server;
use std::env;
use std::time::Duration;

mod core;
mod handler;
mod logger;

// build.rs 保证发布二进制自带可直启默认值；同名运行环境变量仍可用于部署轮换。
const LOG_LEVEL: &str = env!("LOG_LEVEL");
const DEFAULT_BIND_HOST: &str = env!("CTRL_SERVER_DEFAULT_BIND_HOST");
const DEFAULT_SERVER_PORT: &str = env!("CTRL_SERVER_DEFAULT_PORT");
const DEFAULT_TLS_PORT: &str = env!("CTRL_SERVER_DEFAULT_TLS_PORT");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let bind_host = env::var("CTRL_SERVER_BIND_HOST")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_BIND_HOST.to_string());
    let server_port = env::var("CTRL_SERVER_PORT")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_SERVER_PORT.to_string());

    let config = logger::LogConfig {
        dir: std::path::PathBuf::from("./logs"),
        prefix: "ctrl_server".to_string(),
        default_filter: LOG_LEVEL.to_string(),
    };
    logger::init_logging_with_config(config)?;
    color_backtrace::install();
    // 安全材料优先使用运行时路径/变量；缺失时再解密 build.rs 注入的单文件默认值。
    let mut security = SecurityConfig::kik();
    security.tls_port = env_or_default("CTRL_SERVER_TLS_PORT", DEFAULT_TLS_PORT);
    security.server_cert_path = env_optional("CTRL_SERVER_TLS_CERT");
    security.server_cert_pem = if security.server_cert_path.is_none() {
        decode_optional_base64(&hidden!(env!("CTRL_SERVER_DEFAULT_TLS_CERT_PEM_BASE64")))?
    } else {
        None
    };
    security.server_key_path = env_optional("CTRL_SERVER_TLS_KEY");
    security.server_key_pem = if security.server_key_path.is_none() {
        decode_optional_base64(&hidden!(env!("CTRL_SERVER_DEFAULT_TLS_KEY_PEM_BASE64")))?
    } else {
        None
    };
    security.kik_noise_private_key = Some(
        env::var("CTRL_SERVER_KIK_NOISE_PRIVATE_KEY")
            .unwrap_or_else(|_| hidden!(env!("CTRL_SERVER_DEFAULT_KIK_NOISE_PRIVATE_KEY"))),
    );
    security.allow_remote_exec = env_bool("CTRL_SERVER_ALLOW_EXEC")
        .unwrap_or_else(|| parse_bool(&hidden!(env!("CTRL_SERVER_DEFAULT_ALLOW_EXEC"))));

    let default_secret = env::var("CTRL_SERVER_AUTH_SECRET")
        .unwrap_or_else(|_| hidden!(env!("CTRL_SERVER_DEFAULT_AUTH_SECRET")));
    let accounts_json = match env_optional("CTRL_SERVER_ACCOUNTS_JSON_BASE64") {
        Some(value) => decode_optional_base64(&value)?,
        None => decode_optional_base64(&hidden!(env!("CTRL_SERVER_DEFAULT_ACCOUNTS_JSON_BASE64")))?,
    };
    let accounts = AccountRegistry::from_json_or_default(accounts_json.as_deref(), default_secret)?;

    server::run(
        Context::init_with_accounts(accounts),
        Config {
            // 服务端认证材料由 AccountRegistry 持有，通用 Config 不再承载租户秘密。
            id: Id::anonymous(),
            server_host: bind_host,
            server_port,
            read_timeout: Duration::from_secs(45),
            write_timeout: Duration::from_secs(45),
            security,
        },
    )
    .await?;
    Ok(())
}

fn env_or_default(name: &str, default: &str) -> String {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn env_optional(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_bool(name: &str) -> Option<bool> {
    env::var(name).ok().map(|value| parse_bool(value.trim()))
}

fn parse_bool(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn decode_optional_base64(encoded: &str) -> anyhow::Result<Option<String>> {
    if encoded.is_empty() {
        return Ok(None);
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| anyhow::anyhow!(hidden!("构建期 TLS PEM Base64 无效")))?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| anyhow::anyhow!(hidden!("构建期 TLS PEM 不是 UTF-8")))
}
