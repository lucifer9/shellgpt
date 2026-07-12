mod ai;
mod cli;
mod config;
mod context;
mod conversation;
mod debug;
mod error;
mod ids;
mod input;
mod local_session;
mod projection;
mod redact;
mod relay;
mod tunnel;

use std::io::Write as _;

#[tokio::main]
async fn main() {
    match run().await {
        Ok(0) => {}
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("{err:#}");
            std::process::exit(1);
        }
    }
}

async fn run() -> anyhow::Result<i32> {
    let command = cli::parse_args(std::env::args_os().skip(1))?;
    match command {
        cli::Command::Version => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        cli::Command::BootstrapSh => {
            print!("{}", tunnel::bootstrap::stage1_script());
            Ok(0)
        }
        cli::Command::Local {
            continue_mode,
            prompt_args,
        } => {
            let input = input::compose_prompt(prompt_args, std::io::stdin())?;
            let config = config::AiConfig::from_env()?;
            let context = context::collect_local_context().await;
            let debug = config.debug;
            let client = ai::OpenAiClient::new(config)?;
            let session = local_session::LocalShellSession::new(client, debug);
            let answer = session
                .execute(local_session::LocalRequest {
                    mode: if continue_mode {
                        local_session::RequestMode::Continue
                    } else {
                        local_session::RequestMode::New
                    },
                    context,
                    input,
                })
                .await?;
            print_answer(&answer)?;
            Ok(0)
        }
        cli::Command::TunnelSsh { ssh_args } => {
            let config = config::AiConfig::from_env()?;
            match tunnel::ssh::run_tunnel(ssh_args, config).await? {
                tunnel::ssh::TunnelOutcome::Success => Ok(0),
                tunnel::ssh::TunnelOutcome::SshExited(code) => Ok(code),
            }
        }
    }
}

fn print_answer(answer: &str) -> anyhow::Result<()> {
    print!("{answer}");
    if !answer.ends_with('\n') {
        println!();
    }
    std::io::stdout().flush()?;
    Ok(())
}
