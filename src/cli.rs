use anyhow::{bail, ensure};
use std::ffi::OsString;

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Version,
    BootstrapSh,
    Local {
        continue_mode: bool,
        prompt_args: Vec<String>,
    },
    TunnelSsh {
        ssh_args: Vec<String>,
    },
}

pub fn parse_args<I>(args: I) -> anyhow::Result<Command>
where
    I: IntoIterator<Item = OsString>,
{
    let args = args
        .into_iter()
        .map(|arg| {
            arg.into_string()
                .map_err(|_| anyhow::anyhow!("arguments must be valid UTF-8"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    parse_strings(args)
}

pub fn parse_strings(mut args: Vec<String>) -> anyhow::Result<Command> {
    if args.is_empty() {
        return Ok(Command::Local {
            continue_mode: false,
            prompt_args: Vec::new(),
        });
    }
    match args[0].as_str() {
        "--version" => {
            ensure!(args.len() == 1, "--version does not accept arguments");
            Ok(Command::Version)
        }
        "bootstrap" => {
            ensure!(args == ["bootstrap", "sh"], "usage: sgpt bootstrap sh");
            Ok(Command::BootstrapSh)
        }
        "tunnel" => {
            ensure!(
                args.len() >= 2 && args[1] == "ssh",
                "usage: sgpt tunnel ssh [ssh args...]"
            );
            Ok(Command::TunnelSsh {
                ssh_args: args.split_off(2),
            })
        }
        "-c" | "--continue" => {
            args.remove(0);
            if args.first().is_some_and(|arg| arg == "--") {
                args.remove(0);
            }
            Ok(Command::Local {
                continue_mode: true,
                prompt_args: args,
            })
        }
        "--" => {
            args.remove(0);
            Ok(Command::Local {
                continue_mode: false,
                prompt_args: args,
            })
        }
        first if first.starts_with('-') => {
            bail!("unknown option: {first}. Use sgpt -- {first} for literal prompt text.")
        }
        _ => Ok(Command::Local {
            continue_mode: false,
            prompt_args: args,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_leading_continue_flag_controls_mode() {
        assert_eq!(
            parse_strings(vec!["-c".into(), "make".into(), "shorter".into()]).unwrap(),
            Command::Local {
                continue_mode: true,
                prompt_args: vec!["make".into(), "shorter".into()]
            }
        );
        assert_eq!(
            parse_strings(vec!["say".into(), "-c".into()]).unwrap(),
            Command::Local {
                continue_mode: false,
                prompt_args: vec!["say".into(), "-c".into()]
            }
        );
    }

    #[test]
    fn double_dash_allows_prompt_to_start_with_dash() {
        assert_eq!(
            parse_strings(vec!["--".into(), "-c".into(), "literal".into()]).unwrap(),
            Command::Local {
                continue_mode: false,
                prompt_args: vec!["-c".into(), "literal".into()]
            }
        );
        assert_eq!(
            parse_strings(vec!["-c".into(), "--".into(), "-literal".into()]).unwrap(),
            Command::Local {
                continue_mode: true,
                prompt_args: vec!["-literal".into()]
            }
        );
    }

    #[test]
    fn rejects_unknown_leading_option() {
        let err = parse_strings(vec!["--json".into(), "x".into()])
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown option: --json"));
    }
}
