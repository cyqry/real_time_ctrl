//! 只影响当前控制端进程的本地命令。
//!
//! 选择/查看目标可供 CLI、HTTP、管道共用；只有退出进程命令禁止通过开放 API 调用。

use crate::context::Context;
use crate::input_command::{RemoteResp, RemoteSuccessResp};
use common::command::LocalCommand;

pub async fn execute(context: &Context, cmd: LocalCommand) -> anyhow::Result<RemoteResp> {
    match cmd {
        LocalCommand::LocalExit => local_exit(context).await,
        LocalCommand::LocalUse(id) => {
            let selected = context.select_local_target(&id).await?;
            // 返回本次选择的结果，不能重新读共享状态而混入另一个并发 local_use 的结果。
            Ok(RemoteResp::Success(RemoteSuccessResp::Now(
                ctrl_common::cmd_resp_info::LocalNow::Kik(selected),
            )))
        }
        LocalCommand::LocalNow => Ok(RemoteResp::Success(RemoteSuccessResp::Now(
            context.local_now(),
        ))),
    }
}

async fn local_exit(context: &Context) -> anyhow::Result<RemoteResp> {
    context.agent.clone().write().await.close().await;
    println!("控制结束");
    std::process::exit(0);
}
