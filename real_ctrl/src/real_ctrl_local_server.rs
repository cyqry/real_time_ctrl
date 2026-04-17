use std::env;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use common::config::{Config, Id};
use common::generated::encrypted_strings::{PASSWORD, USER_NAME};
use common::host::get_host;
use crate::context::{Agent, Context};

mod local_server;
mod context;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env::set_var("RUST_LOG", "DEBUG");
    env_logger::init();

    let agent = Arc::new(RwLock::new(
        Agent::create(&Config {
            id: Id {
                username: USER_NAME(),
                password: PASSWORD(),
            },
            server_host: get_host(),
            server_port: "9002".to_string(),
            read_timeout: Duration::from_secs(45),
            write_timeout: Duration::from_secs(45),
        })
            .await?,
    ));

    let context = Context::new(agent);

    context.data_init().await?;

    local_server::server::server(&context).await?;
    Ok(())
}