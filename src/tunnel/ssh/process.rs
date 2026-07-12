use super::policy::{
    SSH_CONFIG_LIMIT, SSH_DIAGNOSTIC_LIMIT, ValidatedArgs, add_controlled_options,
};
use anyhow::{Context as _, bail, ensure};
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::mpsc;

pub(super) async fn effective_config(args: &ValidatedArgs) -> anyhow::Result<String> {
    effective_config_with_program(Path::new("ssh"), args).await
}

pub(super) async fn run(
    args: &ValidatedArgs,
    port: u16,
    remote_command: &str,
) -> anyhow::Result<ExitStatus> {
    run_with_program(Path::new("ssh"), args, port, remote_command).await
}

async fn effective_config_with_program(
    program: &Path,
    args: &ValidatedArgs,
) -> anyhow::Result<String> {
    let mut command = tokio::process::Command::new(program);
    command.arg("-G");
    add_controlled_options(&mut command);
    command
        .args(args.as_slice())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("failed to start ssh -G")?;
    let stdout = child.stdout.take().context("ssh -G stdout was not piped")?;
    let stderr = child.stderr.take().context("ssh -G stderr was not piped")?;
    let (failure_tx, mut failure_rx) = mpsc::channel(2);
    let stdout_task = tokio::spawn(read_bounded(
        stdout,
        SSH_CONFIG_LIMIT,
        "ssh -G output exceeded 256 KiB limit.",
        failure_tx.clone(),
    ));
    let stderr_task = tokio::spawn(read_bounded(
        stderr,
        SSH_DIAGNOSTIC_LIMIT,
        "ssh -G stderr exceeded 256 KiB limit.",
        failure_tx,
    ));
    let (status, stream_failure) = tokio::select! {
        status = child.wait() => (status, None),
        failure = failure_rx.recv() => {
            let _ = child.kill().await;
            (child.wait().await, failure)
        }
    };
    let status = status.context("failed to wait for ssh -G")?;
    let bytes = stdout_task.await.context("ssh -G stdout reader failed")??;
    let stderr = stderr_task.await.context("ssh -G stderr reader failed")??;
    let stream_failure = stream_failure.or_else(|| failure_rx.try_recv().ok());
    if let Some(message) = stream_failure {
        bail!(message);
    }
    ensure!(
        status.success(),
        "ssh -G failed: {}",
        String::from_utf8_lossy(&stderr).trim()
    );
    String::from_utf8(bytes).context("ssh -G output must be valid UTF-8")
}

async fn run_with_program(
    program: &Path,
    args: &ValidatedArgs,
    port: u16,
    remote_command: &str,
) -> anyhow::Result<ExitStatus> {
    let mut command = tokio::process::Command::new(program);
    add_controlled_options(&mut command);
    let forwarding = format!("127.0.0.1:{port}:127.0.0.1:{port}");
    command
        .args(["-R", &forwarding])
        .args(args.as_slice())
        .arg(remote_command)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("failed to start ssh")
}

async fn read_bounded(
    mut reader: impl AsyncRead + Unpin,
    limit: usize,
    limit_message: &'static str,
    failure_tx: mpsc::Sender<String>,
) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let count = match reader.read(&mut chunk).await {
            Ok(count) => count,
            Err(err) => {
                let _ = failure_tx
                    .send(format!("ssh -G output read failed: {err}"))
                    .await;
                return Err(err);
            }
        };
        if count == 0 {
            return Ok(output);
        }
        if output.len() + count > limit {
            let remaining = limit.saturating_sub(output.len());
            output.extend_from_slice(&chunk[..remaining]);
            let _ = failure_tx.send(limit_message.to_string()).await;
            return Ok(output);
        }
        output.extend_from_slice(&chunk[..count]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tunnel::ssh::{policy, shell_quote};
    use std::os::unix::fs::PermissionsExt;
    use tokio::time::{Duration, timeout};

    fn args(values: &[&str]) -> ValidatedArgs {
        policy::validate(
            &values
                .iter()
                .map(|value| (*value).to_string())
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    fn fake_ssh(temp: &tempfile::TempDir, script: &str) -> std::path::PathBuf {
        let path = temp.path().join("ssh");
        std::fs::write(&path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn ssh_config_reads_stdout_and_stderr_concurrently() {
        let temp = tempfile::tempdir().unwrap();
        let diagnostic = temp.path().join("diagnostic");
        std::fs::write(&diagnostic, vec![b'e'; SSH_DIAGNOSTIC_LIMIT - 1024]).unwrap();
        let ssh = fake_ssh(
            &temp,
            &format!(
                "cat {} >&2\nprintf 'hostname example\\n'",
                shell_quote(&diagnostic.display().to_string())
            ),
        );
        let result = timeout(
            Duration::from_secs(5),
            effective_config_with_program(&ssh, &args(&["host"])),
        )
        .await
        .expect("ssh -G must not deadlock")
        .unwrap();
        assert_eq!(result, "hostname example\n");
    }

    #[tokio::test]
    async fn ssh_config_bounds_both_streams_and_reaps_child() {
        for stream in ["stdout", "stderr"] {
            let temp = tempfile::tempdir().unwrap();
            let oversized = temp.path().join("oversized");
            let pid = temp.path().join("pid");
            std::fs::write(&oversized, vec![b'x'; SSH_CONFIG_LIMIT + 1]).unwrap();
            let redirect = if stream == "stderr" { ">&2" } else { "" };
            let ssh = fake_ssh(
                &temp,
                &format!(
                    "printf '%s' $$ > {}\ncat {} {}\nexec sleep 30",
                    shell_quote(&pid.display().to_string()),
                    shell_quote(&oversized.display().to_string()),
                    redirect
                ),
            );
            let err = timeout(
                Duration::from_secs(5),
                effective_config_with_program(&ssh, &args(&["host"])),
            )
            .await
            .expect("oversized ssh output must fail promptly")
            .unwrap_err();
            assert!(err.to_string().contains("exceeded 256 KiB"));
            let child_pid = std::fs::read_to_string(pid).unwrap();
            let reaped = std::process::Command::new("kill")
                .args(["-0", child_pid.trim()])
                .stderr(Stdio::null())
                .status()
                .unwrap();
            assert!(!reaped.success(), "ssh -G child was not reaped");
        }
    }

    #[tokio::test]
    async fn ssh_config_nonzero_exit_preserves_bounded_diagnostic() {
        let temp = tempfile::tempdir().unwrap();
        let ssh = fake_ssh(&temp, "printf 'stable diagnostic' >&2\nexit 42");
        let err = effective_config_with_program(&ssh, &args(&["host"]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("stable diagnostic"));
    }

    #[tokio::test]
    async fn ssh_config_rejects_invalid_utf8() {
        let temp = tempfile::tempdir().unwrap();
        let ssh = fake_ssh(&temp, "printf '\\377'");
        let err = effective_config_with_program(&ssh, &args(&["host"]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("valid UTF-8"));
    }

    #[tokio::test]
    async fn final_process_receives_controlled_options_forward_and_validated_args() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("args");
        let ssh = fake_ssh(
            &temp,
            &format!(
                "for arg in \"$@\"; do printf '%s\\n' \"$arg\"; done > {}",
                shell_quote(&log.display().to_string())
            ),
        );
        let status = run_with_program(
            &ssh,
            &args(&["-p", "2222", "host"]),
            18080,
            "remote command",
        )
        .await
        .unwrap();
        assert!(status.success());
        let captured = std::fs::read_to_string(log).unwrap();
        for option in policy::CONTROLLED_OPTIONS {
            assert!(captured.contains(option));
        }
        assert!(captured.contains("127.0.0.1:18080:127.0.0.1:18080"));
        assert!(captured.ends_with("remote command\n"));
    }
}
