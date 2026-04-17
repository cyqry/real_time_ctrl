use crate::context::{id, Context};
use common::command::{Command, SysCommand};
use common::protocol::{CmdOptions, ReqCmd};
use ctrl_common::ctrl_resp::{CmdResp, Resp, ServerResp, ServerSuccessResp};
use crate::input_command::RemoteResp;

pub async fn execute(context: &Context, cmd: SysCommand) -> anyhow::Result<RemoteResp> {
    match context
        .agent
        .write()
        .await
        .req(&ReqCmd::new(id(), CmdOptions::default(), Command::Sys(cmd)))
        .await?
        .get_resp()
    {
        Resp::Server(ServerResp::Success(ServerSuccessResp::Info(info))) => {
            Ok(RemoteResp::Success(info.to_string()))
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
