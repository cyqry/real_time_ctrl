use crate::context::{Agent, Context};
use common::command::Command;
use common::config::{Config, Id};
use log::{debug, error, info};
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use std::env;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use common::generated::encrypted_strings;

mod context;
mod ctrl_conn;
mod ctrl_data_conn;
mod ctrl_executor;
mod direct_executor;
mod dispatch;
mod local_executor;
mod server_executor;

#[tokio::main]
async fn main() {
    env::set_var("RUST_LOG", "DEBUG");
    env_logger::init();

    let agent = Arc::new(RwLock::new(
        Agent::create(&Config {
            id: Id {
                username: "root".to_string(),
                password: "1104399".to_string(),
            },
            server_host: "ytycc.com".to_string(),
            server_port: "9002".to_string(),
        })
            .await
            .unwrap(),
    ));

    let context = Context::new(agent);

    context.data_init().await.unwrap();

    loop {
        let mut s = String::new();
        let n = std::io::stdin().read_line(&mut s).unwrap();
        if s.trim().is_empty() {
            continue;
        }
        debug!("\ninput:{}", s.trim());
        match dispatch::distribution(&context, &s).await {
            Ok(s) => {
                println!("{}", s);
            }
            Err(e) => {
                println!("{}", e);
            }
        }
    }
}

fn test() -> anyhow::Result<()> {
    // `()` can be used when no completer is required
    let mut rl = DefaultEditor::new()?;
    #[cfg(feature = "with-file-history")]
    if rl.load_history("history.txt").is_err() {
        println!("No previous history.");
    }
    loop {
        let readline = rl.readline(">> ");
        match readline {
            Ok(line) => {
                rl.add_history_entry(line.as_str())?;
                println!("Line: {}", line);
            }
            Err(ReadlineError::Interrupted) => {
                println!("CTRL-C");
                break;
            }
            Err(ReadlineError::Eof) => {
                println!("CTRL-D");
                break;
            }
            Err(err) => {
                println!("Error: {:?}", err);
                break;
            }
        }
    }
    #[cfg(feature = "with-file-history")]
    rl.save_history("history.txt");
    Ok(())
}
