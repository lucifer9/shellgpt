pub(super) mod policy;
mod process;

use crate::config::{AiConfig, parse_tunnel_port};
use crate::ids;
use crate::relay::{RelayState, app, bind_loopback};
use anyhow::ensure;
use base64::Engine;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TunnelOutcome {
    Success,
    SshExited(i32),
}

pub async fn run_tunnel(ssh_args: Vec<String>, config: AiConfig) -> anyhow::Result<TunnelOutcome> {
    let validated = policy::validate(&ssh_args)?;
    let user_port = parse_tunnel_port(std::env::var("SGPT_PORT").ok())?;
    let listener = bind_loopback(user_port).await?;
    let actual_port = listener.local_addr()?.port();
    let effective = process::effective_config(&validated).await?;
    policy::validate_effective(&validated, &effective, Some(actual_port))?;

    let token = ids::session_token()?;
    let session_id = ids::id128()?;
    let remote_command =
        remote_bootstrap_command(actual_port, &token, &session_id, config.timeout.as_secs())?;
    let relay_state =
        RelayState::new(token.clone(), config.clone())?.with_bootstrap_port(actual_port);
    relay_state.create_root(session_id).await?;

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

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app(relay_state)).await;
    });
    let status = process::run(&validated, actual_port, &remote_command).await;
    server.abort();
    let _ = server.await;
    let status = status?;
    if status.success() {
        Ok(TunnelOutcome::Success)
    } else {
        Ok(TunnelOutcome::SshExited(status.code().unwrap_or(255)))
    }
}

pub fn validate_effective_policy(
    user_args: &[String],
    effective: &str,
    relay_port: Option<u16>,
) -> anyhow::Result<()> {
    let validated = policy::validate(user_args)?;
    policy::validate_effective(&validated, effective, relay_port)
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
    let script = super::bootstrap::stage1_script();
    let b64 = base64::engine::general_purpose::STANDARD.encode(script.as_bytes());
    let mut env = vec![
        ("SGPT_PORT", port.to_string()),
        ("SGPT_SESSION_TOKEN", token.to_string()),
        ("SGPT_SESSION_ID", session_id.to_string()),
        ("SGPT_TIMEOUT_SECONDS", timeout_seconds.to_string()),
        ("SGPT_VERSION", env!("CARGO_PKG_VERSION").to_string()),
        ("SGPT_STAGE1_B64", b64),
    ];
    if std::env::var("SGPT_DEBUG").ok().as_deref() == Some("1") {
        env.push(("SGPT_DEBUG", "1".into()));
    }
    let assignments = env
        .into_iter()
        .map(|(key, value)| format!("{key}={}", shell_quote(&value)))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(format!(
        "{assignments} sh -c 'sh -c \"$(printf %s \"$SGPT_STAGE1_B64\" | base64 -d)\"'"
    ))
}

pub fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        "''".into()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_bootstrap_decodes_without_eval() {
        let command =
            remote_bootstrap_command(18080, &"a".repeat(64), "0123456789abcdef", 60).unwrap();
        assert!(!command.contains("eval"));
        assert!(command.contains("base64 -d"));
    }
}
