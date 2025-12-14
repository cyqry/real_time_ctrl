use crate::context::{id, Context};
use common::command::Command;
use common::message::resp::Resp;
use common::protocol::{CmdOptions, ReqCmd};

pub async fn execute(context: &Context, cmd: &String) -> anyhow::Result<String> {
    match context
        .agent
        .clone()
        .write()
        .await
        .req(&ReqCmd::new(id(), CmdOptions::default(), Command::Exec(cmd.to_string())))
        .await?
    {
        Resp::Info(info) => Ok(info),
        Resp::DataId(dataId) => {
            unreachable!("test")
        }
    }
}
