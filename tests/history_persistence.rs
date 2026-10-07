use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

mod common;
use common::{chat_response, read_http_request, write_http_response};

#[test]
fn provider_success_with_persistence_failure_prints_nothing_and_creates_no_current() {
    let temp = tempfile::tempdir().unwrap();
    let cwd = temp.path().join("cwd");
    let runtime = temp.path().join("runtime");
    std::fs::create_dir(&cwd).unwrap();
    std::fs::create_dir(&runtime).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();

    let application_runtime = runtime.join("sgpt");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = read_http_request(&mut stream);
        std::fs::remove_dir_all(&application_runtime).unwrap();
        write_http_response(&mut stream, &chat_response("must not print"));
    });

    let output = Command::new(env!("CARGO_BIN_EXE_sgpt"))
        .arg("hello")
        .current_dir(&cwd)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("SGPT_BASE_URL", format!("http://{address}"))
        .env("SGPT_API_KEY", "key")
        .env("SGPT_MODEL", "model")
        .output()
        .unwrap();
    server.join().unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(!runtime.join("sgpt").exists());
}
