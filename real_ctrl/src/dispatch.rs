use crate::context::Context;
use crate::{ctrl_executor, direct_executor, local_executor, server_executor};
use common::command::Command;

pub async fn distribution(context: &Context, command: &String) -> anyhow::Result<String> {
    let command: Command = command.parse()?;
    match command {
        Command::Sys(sys) => server_executor::execute(context, sys).await,
        Command::Local(local) => local_executor::execute(context, local).await,
        Command::Ctrl(ctrl) => ctrl_executor::execute(context, ctrl).await,
        Command::Exec(cmd) => direct_executor::execute(context, &cmd).await,
    }
}
