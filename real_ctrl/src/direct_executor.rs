//! Exec 与命名任务的控制端命令适配器。
//!
//! 这里只负责构造线上命令和解释响应；远程执行由开放 API 策略与 ctrl_server 策略两层决定。

use crate::context::{id, Context};
use crate::input_command::{RemoteResp, RemoteSuccessResp};
use common::command::Command;
use common::message::kik_resp::{ClientSuccessResp, KikResp};
use common::protocol::{CmdOptions, ReqCmd};
use ctrl_common::ctrl_resp::{Resp, ServerResp, ServerSuccessResp};

pub async fn execute(context: &Context, cmd: &String, target: &str) -> anyhow::Result<RemoteResp> {
    execute_command(context, Command::Exec(cmd.to_string()), target).await
}

/// 命名任务只发送名称，程序路径和参数由服务端配置决定。
pub async fn run_task(context: &Context, name: &str, target: &str) -> anyhow::Result<RemoteResp> {
    if !common::task::valid_task_name(name) {
        anyhow::bail!("任务名格式错误");
    }
    execute_command(context, Command::RunTask(name.to_owned()), target).await
}

async fn execute_command(
    context: &Context,
    command: Command,
    target: &str,
) -> anyhow::Result<RemoteResp> {
    match context
        .request_targeted(&ReqCmd::new(id(), CmdOptions::default(), command), target)
        .await?
        .get_resp()
    {
        Resp::Kik(KikResp::Success(ClientSuccessResp::Info(info)))
        | Resp::Server(ServerResp::Success(ServerSuccessResp::Info(info))) => Ok(
            RemoteResp::Success(RemoteSuccessResp::Info(info.to_string())),
        ),
        Resp::Kik(KikResp::Error(err_code, info)) => {
            Ok(RemoteResp::Error(*err_code as u32, info.to_string()))
        }
        Resp::Server(ServerResp::Error(err_code, info)) => {
            Ok(RemoteResp::Error(*err_code as u32, info.to_string()))
        }
        _ => Err(anyhow::anyhow!("服务端响应类型与远程执行命令不匹配")),
    }
}
