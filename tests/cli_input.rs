use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;

#[test]
fn local_binary_accepts_stdin_only_for_new_and_continue() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel();
    let server = std::thread::spawn(move || {
        for answer in ["first answer", "second answer"] {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_http_request(&mut stream);
            sender.send(request).unwrap();
            let body = format!(r#"{{"choices":[{{"message":{{"content":"{answer}"}}}}]}}"#);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
        }
    });

    let temp = tempfile::tempdir().unwrap();
    let runtime = temp.path().join("runtime");
    std::fs::create_dir(&runtime).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();

    let first = run_sgpt(address, temp.path(), &runtime, &[], b"first stdin");
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&first.stdout), "first answer\n");

    let second = run_sgpt(address, temp.path(), &runtime, &["-c"], b"second stdin");
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&second.stdout), "second answer\n");

    server.join().unwrap();
    let first_request: serde_json::Value =
        serde_json::from_slice(&receiver.recv().unwrap()).unwrap();
    let second_request: serde_json::Value =
        serde_json::from_slice(&receiver.recv().unwrap()).unwrap();
    assert_eq!(
        first_request["messages"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["content"],
        "first stdin"
    );
    let messages = second_request["messages"].as_array().unwrap();
    assert_eq!(messages[messages.len() - 1]["content"], "second stdin");
    assert!(
        messages
            .iter()
            .any(|message| message["content"] == "first stdin")
    );
}

fn run_sgpt(
    address: std::net::SocketAddr,
    cwd: &std::path::Path,
    runtime: &std::path::Path,
    args: &[&str],
    stdin: &[u8],
) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_sgpt"))
        .args(args)
        .current_dir(cwd)
        .env("XDG_RUNTIME_DIR", runtime)
        .env("SGPT_BASE_URL", format!("http://{address}"))
        .env("SGPT_API_KEY", "test-key")
        .env("SGPT_MODEL", "test-model")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

fn read_http_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = String::from_utf8_lossy(&bytes[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            line.strip_prefix("content-length: ")
                .or_else(|| line.strip_prefix("Content-Length: "))
        })
        .unwrap()
        .trim()
        .parse::<usize>()
        .unwrap();
    while bytes.len() < header_end + content_length {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    bytes[header_end..header_end + content_length].to_vec()
}
