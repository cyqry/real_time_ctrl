use rustyline::Cmd;
use serde_json::to_string;
use crate::context::{id, Context};
use common::command::{Command, SysCommand};
use common::protocol::{CmdOptions, ReqCmd};
use ctrl_common::ctrl_resp::{CmdResp, Resp, ServerResp, ServerSuccessResp};
use crate::input_command::{RemoteResp, RemoteSuccessResp};

pub async fn execute(context: &Context, cmd: SysCommand) -> anyhow::Result<RemoteResp> {
    match context
        .agent
        .write()
        .await
        .req(&ReqCmd::new(id(), CmdOptions::default(), Command::Sys(cmd.clone())))
        .await?
        .get_resp()
    {
        Resp::Server(ServerResp::Success(ServerSuccessResp::Info(info))) => {
            Ok(RemoteResp::Success(to_remote_resp(cmd,info)?))
        }
        Resp::Server(ServerResp::Error(err_code, info)) => Ok(RemoteResp::Error(
            err_code.clone() as u32,
            info.to_string(),
        )),
        _ => {
            unreachable!("should not happen")
        }
    }
}


fn to_remote_resp(cmd: SysCommand, info: &String) -> anyhow::Result<RemoteSuccessResp> {
    let res = match cmd {
        SysCommand::List => {
            RemoteSuccessResp::SysList(serde_json::from_str(info)?)
        }
        SysCommand::Use(_) => {
            RemoteSuccessResp::Info(info.to_owned())
        }
        SysCommand::Now => {
            RemoteSuccessResp::Now(serde_json::from_str(info)?)
        }
    };
    Ok(res)
}
