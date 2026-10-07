#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// sgpt only uses XDG_RUNTIME_DIR when the directory is private (mode 0700).
pub fn private_runtime_dir(parent: &Path) -> PathBuf {
    let runtime = parent.join("runtime");
    std::fs::create_dir(&runtime).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
    runtime
}

pub fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
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

pub fn chat_response(answer: &str) -> String {
    format!(r#"{{"choices":[{{"message":{{"content":"{answer}"}}}}]}}"#)
}

pub fn write_http_response(stream: &mut TcpStream, body: &str) {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .unwrap();
}
