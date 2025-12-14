use crate::context::{id, Context};
use common::command::{Command, SysCommand};
use common::message::resp::Resp;
use common::protocol::{CmdOptions, ReqCmd};

pub async fn execute(context: &Context, cmd: SysCommand) -> anyhow::Result<String> {
    match context
        .agent
        .clone()
        .write()
        .await
        .req(&ReqCmd::new(id(), CmdOptions::default(), Command::Sys(cmd)))
        .await?
    {
        Resp::Info(info) => Ok(info),
        Resp::DataId(data_id) => {
            unreachable!("test")
        }
    }
}
