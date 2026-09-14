//! Exec 命令适配器。
//!
//! 这里只负责构造线上命令和解释响应；是否允许 Exec 由开放 API 策略与 ctrl_server 策略两层决定。

use crate::context::{id, Context};
use crate::input_command::{RemoteResp, RemoteSuccessResp};
use common::command::Command;
use common::message::kik_resp::{ClientSuccessResp, KikResp};
use common::protocol::{CmdOptions, ReqCmd};
use ctrl_common::ctrl_resp::{Resp, ServerResp, ServerSuccessResp};

pub async fn execute(context: &Context, cmd: &String) -> anyhow::Result<RemoteResp> {
    match context
        .request(&ReqCmd::new(
            id(),
            CmdOptions::default(),
            Command::Exec(cmd.to_string()),
        ))
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
        _ => Err(anyhow::anyhow!("服务端响应类型与 Exec 命令不匹配")),
    }
}
