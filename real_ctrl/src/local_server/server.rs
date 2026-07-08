use crate::context::{Agent, Context};
use crate::local_server::handle_client::handle_client;
use crate::pipe::pipe_common::PIPE_NAME;
use anyhow::{anyhow, Result};
use common::config::{Config, Id, SecurityConfig};
use common::generated::encrypted_strings::{PASSWORD, USER_NAME};
use common::host::get_host_from_env_or_default;
use interprocess::os::windows::named_pipe::{pipe_mode, tokio::*, PipeListenerOptions};
use interprocess::os::windows::security_descriptor::SecurityDescriptor;
use log::{debug, error, info};
use std::sync::Arc;
use std::time::Duration;
use std::{env, io};
use tokio::sync::RwLock;
use widestring::U16CString;

const PIPE_SECURITY_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)";

pub async fn create_context() -> anyhow::Result<Context> {
    let server_port = env::var("REAL_CTRL_SERVER_PORT")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "9002".to_string());

    let agent = Arc::new(RwLock::new(
        Agent::create(&Config {
            id: Id {
                username: USER_NAME(),
                password: PASSWORD(),
            },
            server_host: get_host_from_env_or_default("REAL_CTRL_SERVER_HOST"),
            server_port,
            read_timeout: Duration::from_secs(45),
            write_timeout: Duration::from_secs(45),
            security: SecurityConfig::real_ctrl_from_env(),
        })
        .await?,
    ));

    let context = Context::new(agent);
    context.data_init().await?;
    debug!("real_ctrl 连接初始化成功");
    Ok(context)
}

pub async fn start_pipe_server() -> anyhow::Result<()> {
    let context = create_context().await?;
    server(&context).await?;
    Ok(())
}

pub async fn server(context: &Context) -> Result<()> {
    let listener = match PipeListenerOptions::new()
        .path(std::path::Path::new(PIPE_NAME))
        .accept_remote(false)
        .inheritable(false)
        .security_descriptor(Some(pipe_security_descriptor()?))
        .create_tokio_duplex::<pipe_mode::Bytes>()
    {
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            error!("命名管道已被占用，请检查 {PIPE_NAME} 是否被其他 real_ctrl 进程使用");
            return Err(anyhow!(e));
        }
        x => x?,
    };

    info!("real_ctrl 本地管道已启动: {}", PIPE_NAME);

    loop {
        let stream = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                error!("管道连接接收失败: {}", e);
                continue;
            }
        };

        let context = context.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(context, stream).await {
                error!("处理管道请求失败: {}", e);
            };
        });
    }
}

fn pipe_security_descriptor() -> anyhow::Result<SecurityDescriptor> {
    // 仅允许 System、Administrators 和对象 Owner 访问本地管道，避免同机其他账号直接调用控制 API。
    let sddl = U16CString::from_str(PIPE_SECURITY_SDDL)?;
    Ok(SecurityDescriptor::deserialize(sddl.as_ucstr())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipe_security_sddl_is_valid() {
        pipe_security_descriptor().unwrap();
    }
}
