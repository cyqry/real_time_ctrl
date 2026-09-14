//! 由 ctrl_server 自身处理的系统命令适配器。
//!
//! 在线列表、当前目标和上下线历史不会发送给 Kik；服务端返回的结构化 JSON 在这里恢复为控制端业务类型。

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

fn to_remote_resp(cmd: SysCommand, info: &String) -> anyhow::Result<RemoteSuccessResp> {
    let res = match cmd {
        SysCommand::List => RemoteSuccessResp::SysList(serde_json::from_str(info)?),
        SysCommand::Use(_) => RemoteSuccessResp::Info(info.to_owned()),
        SysCommand::Now => RemoteSuccessResp::Now(serde_json::from_str(info)?),
        SysCommand::History(_) => RemoteSuccessResp::History(serde_json::from_str(info)?),
    };
    Ok(res)
}
