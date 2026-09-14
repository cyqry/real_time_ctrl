//! 只影响当前控制端进程的本地命令。
//!
//! 此类命令不能进入远程线协议，也不允许经 HTTP/命名管道绕过服务层执行。

use crate::context::Context;
use common::command::LocalCommand;

pub async fn execute(context: &Context, cmd: LocalCommand) -> anyhow::Result<String> {
    match cmd {
        LocalCommand::LocalExit => local_exit(context).await,
    }
}

async fn local_exit(context: &Context) -> anyhow::Result<String> {
    context.agent.clone().write().await.close().await;
    println!("控制结束");
    std::process::exit(0);
}
