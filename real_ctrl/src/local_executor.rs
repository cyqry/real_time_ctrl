use crate::context::Context;
use common::command::LocalCommand;
use common::global_const::LOCAL_PREFIX;

pub async fn execute(context: &Context, cmd: LocalCommand) -> anyhow::Result<String> {
    match cmd {
        LocalCommand::LocalExit => local_exit(context).await,
    }
}

async fn local_exit(context: &Context) -> anyhow::Result<String> {
    context.agent.clone().write().await.close().await;
    println!("控制结束");
    std::process::exit(0);
}
