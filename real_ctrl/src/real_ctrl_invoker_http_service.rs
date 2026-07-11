#![windows_subsystem = "windows"]

use anyhow::Result;
use chrono::Local;
use log::error;
use real_ctrl::api_service::RealCtrlApi;
use real_ctrl::http_service::routes;
use real_ctrl::local_server::server::{create_context, server as run_pipe_server};
use real_ctrl::run_util::single;
use spring::config::{ConfigRegistry, Configurable};
use spring::plugin::MutableComponentRegistry;
use spring::{auto_config, App};
use spring_web::WebConfigurator;
use spring_web::WebPlugin;
use std::env;
use std::io::Write;
use std::net::IpAddr;

#[derive(serde::Deserialize)]
struct HttpExposureConfig {
    binding: IpAddr,
}

impl Configurable for HttpExposureConfig {
    fn config_prefix() -> &'static str {
        "web"
    }
}

const LOG_LEVEL: &str = env!("LOG_LEVEL");

#[auto_config(WebConfigurator)]
#[tokio::main]
async fn main() -> Result<()> {
    let lock_path = env::var("REAL_CTRL_HTTP_LOCK_PATH")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "target/runtime/real_ctrl_invoker_http_service.lock".to_string());
    let _single_lock = single(lock_path).await?;
    env::set_var("RUST_LOG", LOG_LEVEL);
    env_logger::Builder::new()
        .format(|buf, record| {
            writeln!(
                buf,
                "{} [{}] - {}",
                Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                record.level(),
                record.args()
            )
        })
        .parse_default_env()
        .init();

    let context = create_context().await?;
    let pipe_context = context.clone();
    let api = RealCtrlApi::new(context);

    tokio::spawn(async move {
        if let Err(e) = run_pipe_server(&pipe_context).await {
            error!("本地管道服务结束: {}", e);
        }
    });

    let mut app = App::new();
    validate_http_exposure(&app)?;
    app.add_component(api)
        .add_router(routes::router())
        .add_plugin(WebPlugin)
        .run()
        .await;
    Ok(())
}

fn validate_http_exposure(app: &impl ConfigRegistry) -> anyhow::Result<()> {
    let exposure = app.get_config::<HttpExposureConfig>()?;
    let token_len = env::var("REAL_CTRL_API_TOKEN")
        .ok()
        .map(|token| token.trim().len())
        .unwrap_or_default();
    validate_http_binding(exposure.binding, token_len)
}

fn validate_http_binding(binding: IpAddr, token_len: usize) -> anyhow::Result<()> {
    if !binding.is_loopback() && token_len < 32 {
        return Err(anyhow::anyhow!(
            "HTTP 绑定非 loopback 地址时，REAL_CTRL_API_TOKEN 至少需要 32 个字符"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_http_binding;

    #[test]
    fn non_loopback_binding_requires_strong_token() {
        assert!(validate_http_binding("127.0.0.1".parse().unwrap(), 0).is_ok());
        assert!(validate_http_binding("0.0.0.0".parse().unwrap(), 31).is_err());
        assert!(validate_http_binding("0.0.0.0".parse().unwrap(), 32).is_ok());
    }
}
