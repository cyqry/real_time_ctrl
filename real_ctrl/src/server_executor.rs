use crate::context::Context;
use common::command::{Command, SysCommand};
use common::message::resp::Resp;

pub async fn execute(context: &Context, cmd: SysCommand) -> anyhow::Result<String> {
    match context
        .agent
        .clone()
        .write()
        .await
        .req(&Command::Sys(cmd))
        .await?
    {
        Resp::Info(info) => Ok(info),
        Resp::DataId(data_id) => {
            unreachable!("test")
        }
    }
}
