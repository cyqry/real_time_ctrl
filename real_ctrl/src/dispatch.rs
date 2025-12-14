use crate::context::Context;
use crate::{ctrl_executor, direct_executor, local_executor, server_executor};
use common::command::Command;
use crate::input_command::InputCommand;

pub async fn distribution(context: &Context, command: &String) -> anyhow::Result<String> {
    let command: InputCommand = command.parse()?;
    match command {
        InputCommand::Sys(sys) => server_executor::execute(context, sys).await,
        InputCommand::Local(local) => local_executor::execute(context, local).await,
        InputCommand::Ctrl(ctrl) => ctrl_executor::execute(context, ctrl).await,
        InputCommand::Exec(cmd) => direct_executor::execute(context, &cmd).await,
    }
}
