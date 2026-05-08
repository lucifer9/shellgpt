use crate::config::{AiConfig, parse_tunnel_port};
use crate::ids;
use crate::relay::{RelayState, app, bind_loopback};
use anyhow::{Context as _, bail, ensure};
use base64::Engine;
use std::process::Stdio;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshInvocation {
    pub argv: Vec<String>,
    pub destination: String,
}

pub async fn run_tunnel(ssh_args: Vec<String>, config: AiConfig) -> anyhow::Result<()> {
    let user_port = parse_tunnel_port(std::env::var("SGPT_PORT").ok())?;
    let listener = bind_loopback(user_port).await?;
    let actual_port = listener.local_addr()?.port();
    let token = ids::session_token()?;
    let session_id = ids::id128()?;
    if config.debug {
        crate::debug::log("relay_addr", format!("127.0.0.1:{actual_port}"));
        crate::debug::log(
            "SGPT_SESSION_TOKEN",
            crate::redact::fingerprint_token(&token),
        );
        crate::debug::log(
            "SGPT_API_KEY",
            crate::redact::fingerprint_secret(&config.api_key),
        );
        crate::debug::log("ai_endpoint", &config.endpoint);
        crate::debug::log("ai_model", &config.model);
        if let Some(proxy) = &config.proxy {
            crate::debug::log("SGPT_PROXY", crate::redact::redact_proxy_password(proxy));
        }
    }
    let remote_command =
        remote_bootstrap_command(actual_port, &token, &session_id, config.timeout.as_secs())?;
    let invocation = build_ssh_invocation(&ssh_args, actual_port, remote_command)?;

    let relay_state = RelayState::new(token, config)?;
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app(relay_state)).await;
    });

    let mut child = tokio::process::Command::new("ssh")
        .args(invocation.argv.iter().skip(1))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to start ssh")?;
    let status = child.wait().await?;
    server.abort();
    if !status.success() {
        eprintln!(
            "Remote port forwarding failed. Check AllowTcpForwarding and PermitOpen settings on the SSH server."
        );
        std::process::exit(status.code().unwrap_or(255));
    }
    Ok(())
}

pub fn build_ssh_invocation(
    user_args: &[String],
    port: u16,
    remote_command: String,
) -> anyhow::Result<SshInvocation> {
    validate_ssh_args(user_args)?;
    let destination = destination_from_args(user_args)?.to_string();
    let mut argv = vec![
        "ssh".into(),
        "-tt".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-R".into(),
        format!("127.0.0.1:{port}:127.0.0.1:{port}"),
    ];
    argv.extend_from_slice(user_args);
    argv.push(remote_command);
    Ok(SshInvocation { argv, destination })
}

fn validate_ssh_args(args: &[String]) -> anyhow::Result<()> {
    ensure!(!args.is_empty(), "sgpt tunnel ssh requires a destination.");
    let mut after_double_dash = false;
    let mut destination_seen = false;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if after_double_dash {
            if !destination_seen {
                destination_seen = true;
                i += 1;
                continue;
            }
            bail!("remote commands are not supported in v1.");
        }
        if arg == "--" {
            after_double_dash = true;
            i += 1;
            continue;
        }
        if !destination_seen && arg == "-T" {
            bail!("sgpt tunnel ssh requires an interactive TTY; -T is not supported.");
        }
        if !destination_seen
            && (arg == "-o"
                || arg == "-R"
                || arg == "-p"
                || arg == "-J"
                || arg == "-i"
                || arg == "-l")
        {
            let value = args.get(i + 1).context("ssh option requires a value")?;
            if arg == "-o" && value.eq_ignore_ascii_case("ExitOnForwardFailure=no") {
                bail!("ExitOnForwardFailure=no is not allowed.");
            }
            i += 2;
            continue;
        }
        if !destination_seen && arg.starts_with("-o") && arg.len() > 2 {
            if arg[2..].eq_ignore_ascii_case("ExitOnForwardFailure=no") {
                bail!("ExitOnForwardFailure=no is not allowed.");
            }
            i += 1;
            continue;
        }
        if !destination_seen && arg.starts_with('-') {
            i += 1;
            continue;
        }
        if !destination_seen {
            destination_seen = true;
            i += 1;
            continue;
        }
        bail!("remote commands are not supported in v1.");
    }
    ensure!(destination_seen, "sgpt tunnel ssh requires a destination.");
    Ok(())
}

fn destination_from_args(args: &[String]) -> anyhow::Result<&str> {
    let mut after_double_dash = false;
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if after_double_dash {
            return Ok(arg);
        }
        if arg == "--" {
            after_double_dash = true;
            i += 1;
            continue;
        }
        if matches!(arg.as_str(), "-o" | "-R" | "-p" | "-J" | "-i" | "-l") {
            i += 2;
            continue;
        }
        if arg.starts_with('-') {
            i += 1;
            continue;
        }
        return Ok(arg);
    }
    bail!("sgpt tunnel ssh requires a destination.")
}

pub fn remote_bootstrap_command(
    port: u16,
    token: &str,
    session_id: &str,
    timeout_seconds: u64,
) -> anyhow::Result<String> {
    ensure!(ids::is_session_token(token), "invalid session token");
    ensure!(ids::is_hex_id(session_id), "invalid session id");
    ensure!((1..=600).contains(&timeout_seconds), "invalid timeout");
    let b64 = base64::engine::general_purpose::STANDARD.encode(super::bootstrap::stage1_script());
    ensure!(
        b64.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'\n')),
        "invalid bootstrap base64"
    );
    let mut env = vec![
        ("SGPT_PORT", port.to_string()),
        ("SGPT_SESSION_TOKEN", token.to_string()),
        ("SGPT_SESSION_ID", session_id.to_string()),
        ("SGPT_TIMEOUT_SECONDS", timeout_seconds.to_string()),
        ("SGPT_VERSION", env!("CARGO_PKG_VERSION").to_string()),
    ];
    if std::env::var("SGPT_DEBUG").ok().as_deref() == Some("1") {
        env.push(("SGPT_DEBUG", "1".into()));
    }
    if std::env::var("SGPT_ACCEPT_INSECURE_RUNTIME_DIR")
        .ok()
        .as_deref()
        == Some("1")
    {
        env.push(("SGPT_ACCEPT_INSECURE_RUNTIME_DIR", "1".into()));
    }
    env.push(("SGPT_STAGE1_B64", b64));

    let assignments = env
        .into_iter()
        .map(|(key, value)| format!("{key}={}", shell_quote(&value)))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(format!(
        "{assignments} sh -c 'eval \"$(printf %s \"$SGPT_STAGE1_B64\" | base64 -d)\"'"
    ))
}

pub fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".into();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn builds_controlled_ssh_argv() {
        let invocation =
            build_ssh_invocation(&args(&["-p", "2222", "user@host"]), 4567, "cmd".into()).unwrap();
        assert_eq!(
            &invocation.argv[..6],
            &[
                "ssh",
                "-tt",
                "-o",
                "ExitOnForwardFailure=yes",
                "-R",
                "127.0.0.1:4567:127.0.0.1:4567"
            ]
        );
        assert_eq!(invocation.destination, "user@host");
        assert_eq!(invocation.argv.last().unwrap(), "cmd");
    }

    #[test]
    fn rejects_unsupported_ssh_forms() {
        assert!(build_ssh_invocation(&args(&["-T", "user@host"]), 1, "cmd".into()).is_err());
        assert!(
            build_ssh_invocation(
                &args(&["-o", "ExitOnForwardFailure=no", "user@host"]),
                1,
                "cmd".into()
            )
            .is_err()
        );
        assert!(build_ssh_invocation(&args(&["user@host", "uptime"]), 1, "cmd".into()).is_err());
        assert!(
            build_ssh_invocation(&args(&["--", "user@host", "uptime"]), 1, "cmd".into()).is_err()
        );
        assert!(build_ssh_invocation(&args(&[]), 1, "cmd".into()).is_err());
    }

    #[test]
    fn accepts_double_dash_destination() {
        let invocation =
            build_ssh_invocation(&args(&["--", "user@host"]), 1, "cmd".into()).unwrap();
        assert_eq!(invocation.destination, "user@host");
    }

    #[test]
    fn remote_command_does_not_include_ai_config_names() {
        let command =
            remote_bootstrap_command(2222, &"a".repeat(64), "0123456789abcdef", 60).unwrap();
        assert!(!command.contains("SGPT_API_KEY"));
        assert!(!command.contains("SGPT_BASE_URL"));
        assert!(!command.contains("SGPT_MODEL"));
        assert!(command.contains("SGPT_SESSION_TOKEN"));
    }

    #[test]
    fn remote_command_keeps_ssh_tty_as_shell_stdin() {
        let command =
            remote_bootstrap_command(2222, &"a".repeat(64), "0123456789abcdef", 60).unwrap();
        assert!(command.contains("eval \"$("));
        assert!(!command.contains("| sh"));
    }
}
