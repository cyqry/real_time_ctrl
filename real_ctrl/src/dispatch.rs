use crate::context::Context;
use crate::input_command::{InputCommand, RemoteResp};
use crate::{ctrl_executor, direct_executor, local_executor, server_executor};

pub async fn distribution(context: &Context, command: InputCommand) -> anyhow::Result<String> {
    match command {
        InputCommand::Sys(sys) => match server_executor::execute(context, sys).await? {
            RemoteResp::Success(info) => Ok(info),
            RemoteResp::Error(code, info) => Err(anyhow::anyhow!(info)),
            _ => {
                unreachable!()
            }
        },
        InputCommand::Local(local) => local_executor::execute(context, local).await,
        InputCommand::Ctrl(ctrl) => match ctrl_executor::execute(context, ctrl, false).await? {
            RemoteResp::Success(info) => Ok(info),
            RemoteResp::Error(code, info) => Err(anyhow::anyhow!(info)),
            _ => {
                unreachable!()
            }
        },
        InputCommand::Exec(cmd) => match direct_executor::execute(context, &cmd).await? {
            RemoteResp::Success(info) => Ok(info),
            RemoteResp::Error(code, info) => Err(anyhow::anyhow!(info)),
            _ => {
                unreachable!()
            }
        },
    }
}

pub async fn distribution_other(
    context: &Context,
    command: InputCommand,
) -> anyhow::Result<RemoteResp> {
    match command {
        InputCommand::Sys(sys) => server_executor::execute(context, sys).await,
        InputCommand::Ctrl(ctrl) => ctrl_executor::execute(context, ctrl, true).await,
        InputCommand::Exec(cmd) => direct_executor::execute(context, &cmd).await,
        _ => {
            unimplemented!("不支持的")
        }
    }
}
