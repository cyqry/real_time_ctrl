use crate::context::Context;
use common::command::Command;
use common::message::resp::Resp;

pub async fn execute(context: &Context, cmd: &String) -> anyhow::Result<String> {
    match context
        .agent
        .clone()
        .write()
        .await
        .req(&Command::Exec(cmd.to_string()))
        .await?
    {
        Resp::Info(info) => Ok(info),
        Resp::DataId(dataId) => {
            unreachable!("test")
        }
    }
}
