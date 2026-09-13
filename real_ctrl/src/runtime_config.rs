use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use common::config::{Config, Id, SecurityConfig};
use common::hidden;
use common::host::get_host_from_env_or;
use std::env;
use std::time::Duration;

const DEFAULT_SERVER_HOST: &str = env!("REAL_CTRL_DEFAULT_SERVER_HOST");
const DEFAULT_TLS_PORT: &str = env!("REAL_CTRL_DEFAULT_TLS_PORT");
const DEFAULT_TLS_SERVER_NAME: &str = env!("REAL_CTRL_DEFAULT_TLS_SERVER_NAME");

/// 三个 real_ctrl 进程形态共用同一配置入口，避免 CLI、管道和 HTTP 的默认值发生分叉。
/// 构建默认值保证发布 EXE 可直接运行；部署时仍可用同名运行环境变量覆盖。
pub fn connection_config() -> anyhow::Result<Config> {
    let ca_cert_path = runtime_optional("REAL_CTRL_TLS_CA_CERT");
    let ca_cert_pem = if ca_cert_path.is_none() {
        decode_optional_base64(&hidden!(env!("REAL_CTRL_DEFAULT_TLS_CA_PEM_BASE64")))?
    } else {
        None
    };
    let security = SecurityConfig {
        tls_port: runtime_or_default("REAL_CTRL_TLS_PORT", DEFAULT_TLS_PORT),
        tls_server_name: runtime_or_default("REAL_CTRL_TLS_SERVER_NAME", DEFAULT_TLS_SERVER_NAME),
        pinned_spki_sha256: runtime_or_optional_default(
            "REAL_CTRL_TLS_SERVER_SPKI_SHA256",
            &hidden!(env!("REAL_CTRL_DEFAULT_TLS_SPKI_SHA256")),
        ),
        ca_cert_path,
        ca_cert_pem,
        server_cert_path: None,
        server_cert_pem: None,
        server_key_path: None,
        server_key_pem: None,
        kik_noise_private_key: None,
        allow_remote_exec: false,
    };

    let account_id =
        runtime_or_default("REAL_CTRL_ACCOUNT_ID", env!("REAL_CTRL_DEFAULT_ACCOUNT_ID"));
    let instance_id = runtime_optional("REAL_CTRL_INSTANCE_ID")
        .or_else(|| {
            let compiled = hidden!(env!("REAL_CTRL_DEFAULT_INSTANCE_ID"));
            (!compiled.is_empty()).then_some(compiled)
        })
        // 默认每个进程使用独立实例 ID，同一份 EXE 可同时双击启动且不会互相踢下线。
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    Ok(Config {
        id: Id::control_plane_from_env_or(
            "REAL_CTRL_AUTH_SECRET",
            hidden!(env!("REAL_CTRL_DEFAULT_AUTH_SECRET")),
        )?
        .with_control_identity(account_id, instance_id)?,
        server_host: get_host_from_env_or("REAL_CTRL_SERVER_HOST", DEFAULT_SERVER_HOST),
        // real_ctrl 仅使用 security.tls_port；保留字段为空，避免产生两个端口来源。
        server_port: String::new(),
        read_timeout: Duration::from_secs(45),
        write_timeout: Duration::from_secs(45),
        security,
    })
}

/// 本地 API token 与控制连接配置使用同一覆盖规则，避免 HTTP 入口再次要求启动脚本注入。
pub fn api_token() -> Option<String> {
    runtime_or_optional_default(
        "REAL_CTRL_API_TOKEN",
        &hidden!(env!("REAL_CTRL_DEFAULT_API_TOKEN")),
    )
}

pub fn api_allow_exec() -> bool {
    runtime_bool("REAL_CTRL_API_ALLOW_EXEC")
        .unwrap_or_else(|| parse_bool(&hidden!(env!("REAL_CTRL_DEFAULT_API_ALLOW_EXEC"))))
}

fn runtime_or_default(name: &str, default: &str) -> String {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn runtime_or_optional_default(name: &str, default: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| (!default.is_empty()).then(|| default.to_string()))
}

fn runtime_optional(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn runtime_bool(name: &str) -> Option<bool> {
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
        .map_err(|_| anyhow::anyhow!(hidden!("构建期 TLS CA Base64 无效")))?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| anyhow::anyhow!(hidden!("构建期 TLS CA 不是 UTF-8 PEM")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiled_public_defaults_are_valid() {
        assert!(!DEFAULT_SERVER_HOST.is_empty());
        assert!(DEFAULT_TLS_PORT.parse::<u16>().is_ok());
        assert!(!DEFAULT_TLS_SERVER_NAME.is_empty());
        let pin = hidden!(env!("REAL_CTRL_DEFAULT_TLS_SPKI_SHA256"));
        assert!(pin.is_empty() || pin.len() == 64);
        assert!(hidden!(env!("REAL_CTRL_DEFAULT_AUTH_SECRET")).len() >= 32);
    }
}
