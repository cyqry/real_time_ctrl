use std::error::Error;
use anyhow::{anyhow, Result};
use std::io;
use std::io::{Read, Write};
use std::sync::Arc;
use std::time::Duration;
use anyhow::__private::kind::TraitKind;
use crate::context::{Agent, Context};
use crate::local_server::handle_client::handle_client;
use interprocess::os::windows::named_pipe::{pipe_mode, tokio::*, PipeListenerOptions};
use log::{debug, error, info};
use tokio::sync::RwLock;
use common::config::{Config, Id};
use common::generated::encrypted_strings::{PASSWORD, USER_NAME};
use common::host::get_host;
use crate::local_server;
use crate::pipe::pipe_common::PIPE_NAME;


pub async fn start_pipe_server() -> anyhow::Result<()> {
    let agent = Arc::new(RwLock::new(
        Agent::create(&Config {
            id: Id {
                username: USER_NAME(),
                password: PASSWORD(),
            },
            server_host: get_host(),
            server_port: "9002".to_string(),
            read_timeout: Duration::from_secs(45),
            write_timeout: Duration::from_secs(45),
        })
            .await?,
    ));

    let context = Context::new(agent);

    context.data_init().await?;
    debug!("连接成功");
    local_server::server::server(&context).await?;
    Ok(())
}

pub async fn server(context: &Context) -> Result<()> {

    //  创建命名管道服务器 (异步版)
    let listener = match PipeListenerOptions::new()
        .path(std::path::Path::new(PIPE_NAME))
        .create_tokio_duplex::<pipe_mode::Bytes>()  // 关键：使用异步创建方法
    {
        // 处理 "地址已占用" 的错误
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            error!(
                "Error: could not start server because the socket file is occupied. \
                Please check if {PIPE_NAME} is in use by another process and try again."
            );
            return Err(anyhow!(e));
        }
        x => x?,
    };

    info!("服务端 A 已启动，监听管道: \\\\.\\pipe\\{}", PIPE_NAME);

    // 2. 异步地循环处理连接
    loop {
        // 异步等待连接
        let stream = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                error!("连接接受失败: {}", e);
                continue;
            }
        };

        let context = context.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(context, stream).await {
                error!("处理请求失败: {}", e);
            };
        });
    }
}
