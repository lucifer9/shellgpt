use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

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
        let mut request = [0_u8; 8192];
        let _ = stream.read(&mut request).unwrap();
        std::fs::remove_dir_all(&application_runtime).unwrap();
        let body = br#"{"choices":[{"message":{"content":"must not print"}}]}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
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
