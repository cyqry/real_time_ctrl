use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use common::config::{Config, Id, SecurityConfig};
use common::hidden;
use core::context::Context;
use core::server;
use std::env;
use std::time::Duration;

mod core;
mod handler;
mod logger;

//编译期获取环境变量，写死在程序
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
    // 进程级别钩子
    // panic::set_hook(Box::new(|panic_info| {
    //     // 获取 backtrace
    //     let backtrace = Backtrace::capture();
    //     error!("panic_info:{:?}", panic_info);
    //
    // }));
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

    server::run(
        Context::init(),
        Config {
            id: Id::control_plane_from_env_or(
                "CTRL_SERVER_AUTH_SECRET",
                hidden!(env!("CTRL_SERVER_DEFAULT_AUTH_SECRET")),
            )?,
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
