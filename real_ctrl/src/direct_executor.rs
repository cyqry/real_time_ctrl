use crate::context::{id, Context};
use common::command::Command;
use common::message::kik_resp::{ClientSuccessResp, KikResp};
use common::protocol::{CmdOptions, ReqCmd};
use ctrl_common::ctrl_resp::{Resp, ServerResp, ServerSuccessResp};
use crate::input_command::{RemoteResp, RemoteSuccessResp};

pub async fn execute(context: &Context, cmd: &String) -> anyhow::Result<RemoteResp> {
    match context
        .agent
        .clone()
        .write()
        .await
        .req(&ReqCmd::new(id(), CmdOptions::default(), Command::Exec(cmd.to_string())))
        .await?
        .get_resp()
    {
        Resp::Kik(KikResp::Success(ClientSuccessResp::Info(info))) | Resp::Server(ServerResp::Success(ServerSuccessResp::Info(info))) => {
            Ok(RemoteResp::Success(RemoteSuccessResp::Info(info.to_string())))
        }
        Resp::Kik(KikResp::Error(err_code, info)) => Ok(RemoteResp::Error(
            err_code.clone() as u32,
            info.to_string(),
        )),
        Resp::Server(ServerResp::Error(err_code, info)) => Ok(RemoteResp::Error(
            err_code.clone() as u32,
            info.to_string(),
        )),
        _ => {
            unreachable!("should not happen")
        }
    }
}
