use crate::auth_util;
use anyhow::{anyhow, Context};
use std::env;
use std::string::ToString;
use std::time::Duration;

const DEFAULT_TLS_PORT: &str = env!("RTC_DEFAULT_TLS_PORT");
const DEFAULT_TLS_SERVER_NAME: &str = env!("RTC_DEFAULT_TLS_SERVER_NAME");

#[derive(Clone)]
pub struct Config {
    pub id: Id,
    pub server_host: String,
    pub server_port: String,
    pub read_timeout: Duration,
    pub write_timeout: Duration,
    pub security: SecurityConfig,
}

#[derive(Clone)]
pub struct Id {
    pub username: String,
    pub password: String,
}

impl Id {
    /// 从进程级秘密注入控制面凭据，避免把可复用控制秘密编译进客户端或服务端二进制。
    pub fn control_plane_from_env(name: &str) -> anyhow::Result<Self> {
        let secret =
            env::var(name).with_context(|| format!("缺少控制面认证秘密环境变量 {name}"))?;
        let secret = secret.trim().to_string();
        if secret.len() < 32 {
            return Err(anyhow!("{name} 至少需要 32 个 ASCII 字符"));
        }
        if !secret.is_ascii() {
            return Err(anyhow!(
                "{name} 当前只接受 ASCII，避免跨平台编码产生不同 HMAC"
            ));
        }

        Ok(Self {
            username: "real_ctrl_v2".to_string(),
            password: secret,
        })
    }

    /// v2 challenge/session 直接使用高熵部署秘密作为 HMAC key，不再套用历史摘要算法。
    pub fn control_plane_secret(&self) -> &str {
        &self.password
    }

    /// 仅供显式明文迁移模式兼容旧协议，不能作为抗中间人安全边界。
    pub fn encrypt(&self) -> String {
        auth_util::encrypt(self.username.as_str(), self.password.as_str())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientTransportMode {
    Plain,
    PinnedTls,
}

#[derive(Clone, Debug)]
pub struct SecurityConfig {
    pub client_mode: ClientTransportMode,
    pub tls_port: String,
    pub tls_server_name: String,
    pub pinned_spki_sha256: Option<String>,
    pub ca_cert_path: Option<String>,
    pub server_cert_path: Option<String>,
    pub server_key_path: Option<String>,
    /// 仅迁移期允许服务端明文端口接收 real_ctrl；生产默认必须关闭。
    pub allow_plain_ctrl: bool,
    /// 服务端最终授权开关；开放 API 和被控端命令分派不能绕过它。
    pub allow_remote_exec: bool,
}

impl SecurityConfig {
    pub fn plain() -> Self {
        Self {
            client_mode: ClientTransportMode::Plain,
            tls_port: DEFAULT_TLS_PORT.to_string(),
            tls_server_name: DEFAULT_TLS_SERVER_NAME.to_string(),
            pinned_spki_sha256: None,
            ca_cert_path: None,
            server_cert_path: None,
            server_key_path: None,
            allow_plain_ctrl: false,
            allow_remote_exec: false,
        }
    }

    pub fn real_ctrl_from_env() -> Self {
        let mut config = Self::plain();
        config.client_mode = ClientTransportMode::PinnedTls;
        config.tls_port = env::var("REAL_CTRL_TLS_PORT").unwrap_or(config.tls_port);
        config.tls_server_name =
            env::var("REAL_CTRL_TLS_SERVER_NAME").unwrap_or(config.tls_server_name);
        config.pinned_spki_sha256 = env::var("REAL_CTRL_TLS_SERVER_SPKI_SHA256").ok();
        config.ca_cert_path = env::var("REAL_CTRL_TLS_CA_CERT").ok();

        if env_flag("REAL_CTRL_ALLOW_PLAIN") {
            config.client_mode = ClientTransportMode::Plain;
        }

        config
    }

    pub fn ctrl_server_from_env() -> Self {
        let mut config = Self::plain();
        config.tls_port = env::var("CTRL_SERVER_TLS_PORT").unwrap_or(config.tls_port);
        config.server_cert_path = env::var("CTRL_SERVER_TLS_CERT").ok();
        config.server_key_path = env::var("CTRL_SERVER_TLS_KEY").ok();
        config.allow_plain_ctrl = env_flag("CTRL_SERVER_ALLOW_PLAIN_CTRL");
        config.allow_remote_exec = env_flag("CTRL_SERVER_ALLOW_EXEC");
        config
    }

    pub fn server_tls_enabled(&self) -> bool {
        self.server_cert_path.is_some() || self.server_key_path.is_some()
    }
}

fn env_flag(name: &str) -> bool {
    matches!(
        env::var(name).map(|value| value.to_ascii_lowercase()),
        Ok(value) if matches!(value.as_str(), "1" | "true" | "yes" | "on")
    )
}

#[cfg(test)]
mod tests {
    use super::{Id, SecurityConfig, DEFAULT_TLS_PORT, DEFAULT_TLS_SERVER_NAME};

    #[test]
    fn plain_security_config_uses_build_defaults() {
        let config = SecurityConfig::plain();
        assert_eq!(config.tls_port, DEFAULT_TLS_PORT);
        assert_eq!(config.tls_server_name, DEFAULT_TLS_SERVER_NAME);
    }

    #[test]
    fn control_plane_secret_requires_minimum_length() {
        std::env::set_var("RTC_TEST_SHORT_SECRET", "too-short");
        let result = Id::control_plane_from_env("RTC_TEST_SHORT_SECRET");
        std::env::remove_var("RTC_TEST_SHORT_SECRET");
        assert!(result.is_err());
    }
}
