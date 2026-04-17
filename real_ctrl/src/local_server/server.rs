use std::io;
use std::io::{Read, Write};
use anyhow::{anyhow, Result};

use interprocess::local_socket::{GenericNamespaced, ListenerOptions, ToFsName, ToNsName};
use interprocess::local_socket::traits::ListenerExt;
use interprocess::os::windows::local_socket::NamedPipe;
use crate::context::Context;
use crate::local_server::handle_client::handle_client;

// 定义管道名称（Windows 格式）
const PIPE_NAME: &str = r"\\.\pipe\my_service_pipe";


pub async fn server(context: &Context) -> Result<()> {
    // 创建命名管道服务器
    let name =PIPE_NAME.to_fs_name::<NamedPipe>()?;

    let listener = match ListenerOptions::new().name(name).create_sync() {
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            // When a program that uses a file-type socket name terminates
            // its socket server without deleting the file, a "corpse socket"
            // remains, which can neither be connected to nor reused by a new
            // listener. Normally, Interprocess takes care of this on affected
            // platforms by deleting the socket file when the listener is
            // dropped. (This is vulnerable to all sorts of races and thus can
            // be disabled.)
            //
            // In a real program, instead of leaving it up to the user
            // to perform cleanup, one would use the .try_overwrite(true)
            // listener option to try to replace the socket.
            eprintln!(
                "Error: could not start server because the socket file is \
                occupied. Please check if {PIPE_NAME} is in use by another \
                process and try again."
            );
            return Err(anyhow!(e));
        }
        x => x?,
    };
    println!("服务端 A 已启动，监听管道: {}", PIPE_NAME);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                // 每个连接单独处理（简单场景可以顺序处理，也可 spawn 线程）
                if let Err(e) = handle_client(context, stream).await {
                    eprintln!("处理请求失败: {}", e);
                }
            }
            Err(e) => eprintln!("连接接受失败: {}", e),
        }
    }
    Ok(())
}