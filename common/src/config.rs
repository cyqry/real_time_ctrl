//! 三端共享的运行配置数据结构。
//!
//! 本模块只定义经过校验后供连接层使用的配置，不负责决定配置来源。`real_ctrl`、`ctrl_server` 和
//! `ctrl_kik` 各自在启动层合并编译期默认值与允许运行时覆盖的环境变量。

use crate::hidden;
use anyhow::{anyhow, Context};
use std::env;
use std::string::ToString;
use std::time::Duration;

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
    control_plane_secret: String,
    account_id: String,
    instance_id: String,
}

impl Id {
    /// `ctrl_kik` 不持有控制面凭据，使用显式匿名身份避免空用户名/口令散落在调用点。
    pub fn anonymous() -> Self {
        Self {
            control_plane_secret: String::new(),
            account_id: String::new(),
            instance_id: String::new(),
        }
    }

    /// 从进程级秘密注入控制面凭据。
    pub fn control_plane_from_env(name: &str) -> anyhow::Result<Self> {
        let secret =
            env::var(name).with_context(|| hidden!("缺少控制面认证秘密环境变量 ", name))?;
        Self::control_plane(secret, name)
    }

    /// 运行环境优先，缺失时使用构建期加密写入的部署默认值。默认值只解决单文件直接运行，
    /// 并不具备服务端密钥管理系统的轮换与进程隔离能力。
    pub fn control_plane_from_env_or(name: &str, default: String) -> anyhow::Result<Self> {
        let secret = env::var(name).unwrap_or(default);
        Self::control_plane(secret, name)
    }

    fn control_plane(secret: String, name: &str) -> anyhow::Result<Self> {
        let secret = secret.trim().to_string();
        if secret.len() < 32 {
            return Err(anyhow!(hidden!(name, " 至少需要 32 个 ASCII 字符")));
        }
        if !secret.is_ascii() {
            return Err(anyhow!(hidden!(
                name,
                " 当前只接受 ASCII，避免跨平台编码产生不同 HMAC"
            )));
        }

        Ok(Self {
            control_plane_secret: secret,
            account_id: hidden!("default"),
            instance_id: String::new(),
        })
    }

    /// challenge/session 直接使用高熵部署秘密作为 HMAC key，不再套用静态摘要算法。
    pub fn control_plane_secret(&self) -> &str {
        &self.control_plane_secret
    }

    /// 构造可并存的控制实例身份。账号决定服务端授权域，实例 ID 只用于会话隔离与重连替换。
    pub fn with_control_identity(
        mut self,
        account_id: String,
        instance_id: String,
    ) -> anyhow::Result<Self> {
        validate_identity(&hidden!("account_id"), &account_id)?;
        validate_identity(&hidden!("instance_id"), &instance_id)?;
        self.account_id = account_id;
        self.instance_id = instance_id;
        Ok(self)
    }

    pub fn account_id(&self) -> &str {
        &self.account_id
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }
}

fn validate_identity(name: &str, value: &str) -> anyhow::Result<()> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(anyhow!(hidden!(
            name,
            " 必须是 1..64 位 ASCII 字母、数字、- 或 _"
        )));
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct SecurityConfig {
    pub tls_port: String,
    pub tls_server_name: String,
    pub pinned_spki_sha256: Option<String>,
    pub ca_cert_path: Option<String>,
    /// 与 `ca_cert_path` 二选一；用于无需旁路证书文件即可直接运行的控制端产物。
    pub ca_cert_pem: Option<String>,
    pub server_cert_path: Option<String>,
    /// 与 `server_cert_path` 二选一；运行环境指定路径时必须覆盖该构建默认值。
    pub server_cert_pem: Option<String>,
    pub server_key_path: Option<String>,
    /// 与 `server_key_path` 二选一；只用于用户明确要求的单文件服务端部署。
    pub server_key_pem: Option<String>,
    /// Noise NK 服务端 X25519 私钥；运行环境优先，构建默认值用于直接运行。
    pub kik_noise_private_key: Option<String>,
    /// 服务端最终授权开关；开放 API 和被控端命令分派不能绕过它。
    pub allow_remote_exec: bool,
}

impl SecurityConfig {
    /// `ctrl_kik` 只需要 Noise 公钥侧配置；TLS/服务端私钥字段保持为空。
    pub fn kik() -> Self {
        Self {
            tls_port: String::new(),
            tls_server_name: String::new(),
            pinned_spki_sha256: None,
            ca_cert_path: None,
            ca_cert_pem: None,
            server_cert_path: None,
            server_cert_pem: None,
            server_key_path: None,
            server_key_pem: None,
            kik_noise_private_key: None,
            allow_remote_exec: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Id, SecurityConfig};

    #[test]
    fn kik_security_config_does_not_carry_tls_identity() {
        let config = SecurityConfig::kik();
        assert!(config.tls_port.is_empty());
        assert!(config.tls_server_name.is_empty());
        assert!(config.ca_cert_path.is_none());
        assert!(config.pinned_spki_sha256.is_none());
    }

    #[test]
    fn control_plane_secret_requires_minimum_length() {
        std::env::set_var("RTC_TEST_SHORT_SECRET", "too-short");
        let result = Id::control_plane_from_env("RTC_TEST_SHORT_SECRET");
        std::env::remove_var("RTC_TEST_SHORT_SECRET");
        assert!(result.is_err());
    }
}
