//! 由 ctrl_server 自身处理的系统命令适配器。
//!
//! 在线列表和上下线历史不会发送给 Kik；目标选择由控制端本地状态维护。

use crate::context::{id, Context};
use crate::input_command::{RemoteResp, RemoteSuccessResp};
use common::command::{Command, SysCommand};
use common::protocol::{CmdOptions, ReqCmd};
use ctrl_common::ctrl_resp::{Resp, ServerResp, ServerSuccessResp};

pub async fn execute(context: &Context, cmd: SysCommand) -> anyhow::Result<RemoteResp> {
    match context
        .request(&ReqCmd::new(
            id(),
            CmdOptions::default(),
            Command::Sys(cmd.clone()),
        ))
        .await?
        .get_resp()
    {
        Resp::Server(ServerResp::Success(ServerSuccessResp::Info(info))) => {
            Ok(RemoteResp::Success(to_remote_resp(cmd, info)?))
        }
        Resp::Server(ServerResp::Error(err_code, info)) => {
            Ok(RemoteResp::Error(*err_code as u32, info.to_string()))
        }
        _ => Err(anyhow::anyhow!("服务端响应类型与系统命令不匹配")),
    }
}

fn to_remote_resp(cmd: SysCommand, info: &str) -> anyhow::Result<RemoteSuccessResp> {
    let res = match cmd {
        SysCommand::List => RemoteSuccessResp::SysList(serde_json::from_str(info)?),
        SysCommand::History(_) => RemoteSuccessResp::History(serde_json::from_str(info)?),
    };
    Ok(res)
}

pub async fn online_kiks(
    context: &Context,
) -> anyhow::Result<Vec<ctrl_common::cmd_resp_info::KikInfoVo>> {
    match execute(context, SysCommand::List).await? {
        RemoteResp::Success(RemoteSuccessResp::SysList(kiks)) => Ok(kiks),
        RemoteResp::Error(_, message) => Err(anyhow::anyhow!(message)),
        _ => Err(anyhow::anyhow!("在线列表响应类型不匹配")),
    }
}
