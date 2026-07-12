use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};

fn available_port() -> u16 {
    TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn fake_ssh(temp: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
    let bin = temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let ssh = bin.join("ssh");
    std::fs::write(&ssh, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn tunnel_command(path: &std::path::Path, port: u16) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sgpt"));
    command
        .args(["tunnel", "ssh", "host"])
        .env("PATH", path)
        .env("SGPT_BASE_URL", "http://127.0.0.1:9")
        .env("SGPT_API_KEY", "key")
        .env("SGPT_MODEL", "model")
        .env("SGPT_PORT", port.to_string());
    command
}

fn run_with_exit(exit: &str) -> (Output, u16) {
    let temp = tempfile::tempdir().unwrap();
    let bin = fake_ssh(
        &temp,
        &format!("if [ \"$1\" = -G ]; then printf 'hostname host\\n'; exit 0; fi\n{exit}"),
    );
    let port = available_port();
    let output = tunnel_command(&bin, port).output().unwrap();
    (output, port)
}

#[test]
fn tunnel_preserves_ssh_exit_status_and_releases_relay_listener() {
    for (script, expected) in [("exit 0", 0), ("exit 42", 42), ("exit 255", 255)] {
        let (output, port) = run_with_exit(script);
        assert_eq!(output.status.code(), Some(expected), "{script}");
        TcpListener::bind((Ipv4Addr::LOCALHOST, port)).expect("relay listener must be released");
    }

    let (signalled, port) = run_with_exit("kill -TERM $$");
    assert_eq!(signalled.status.code(), Some(255));
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).expect("relay listener must be released");
}

#[test]
fn remote_bootstrap_failure_is_not_misreported_as_forwarding_failure() {
    let (output, _) = run_with_exit("printf 'sgpt remote dependency missing: jq\\n' >&2; exit 1");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr.contains("sgpt remote dependency missing: jq"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("Remote port forwarding failed"),
        "{stderr}"
    );
}

#[test]
fn tunnel_spawn_failure_returns_one_with_context_and_stops_relay() {
    let temp = tempfile::tempdir().unwrap();
    let bin = fake_ssh(
        &temp,
        "if [ \"$1\" = -G ]; then /bin/rm -- \"$0\"; printf 'hostname host\\n'; exit 0; fi\nexit 0",
    );
    let port = available_port();
    let output = tunnel_command(&bin, port).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("failed to start ssh"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).expect("relay listener must be released");
}
