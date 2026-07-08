use crate::auth_util;
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
    pub username: String,
    pub password: String,
}

impl Id {
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
}

impl SecurityConfig {
    pub fn plain() -> Self {
        Self {
            client_mode: ClientTransportMode::Plain,
            tls_port: "9443".to_string(),
            tls_server_name: "real-ctrl-server".to_string(),
            pinned_spki_sha256: None,
            ca_cert_path: None,
            server_cert_path: None,
            server_key_path: None,
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

pub static DEFAULT_USER_NAME: &str = "user";
pub static DEFAULT_PASS_WARD: &str = "123456";
