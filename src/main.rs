mod ai;
mod cli;
mod config;
mod context;
mod debug;
mod error;
mod history;
mod ids;
mod input;
mod redact;
mod relay;
mod tunnel;

use anyhow::Context as _;

#[tokio::main]
async fn main() {
    if let Err(err) = run().await {
        eprintln!("{err:#}");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let command = cli::parse_args(std::env::args_os().skip(1))?;
    match command {
        cli::Command::Version => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        cli::Command::BootstrapSh => {
            print!("{}", tunnel::bootstrap::stage1_script());
            Ok(())
        }
        cli::Command::Local {
            continue_mode,
            prompt_args,
        } => {
            let input = input::compose_prompt(prompt_args, std::io::stdin())?;
            let config = config::AiConfig::from_env()?;
            let context = context::collect_local_context().await;
            let session = history::LocalSession::resolve().await?;
            let client = ai::OpenAiClient::new(config.clone())?;
            history::run_local_request(&session, &client, &config, &context, continue_mode, input)
                .await
        }
        cli::Command::TunnelSsh { ssh_args } => {
            let config = config::AiConfig::from_env()?;
            tunnel::ssh::run_tunnel(ssh_args, config)
                .await
                .context("tunnel failed")
        }
    }
}
