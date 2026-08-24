use std::io::{self, Read};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const DEADLINE: Duration = Duration::from_secs(10);

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn id(&self) -> u32 {
        self.child.as_ref().unwrap().id()
    }

    fn wait_with_output(mut self, timeout: Duration) -> Output {
        let mut child = self.child.take().unwrap();
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return child.wait_with_output().unwrap(),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let output = child.wait_with_output().unwrap();
                    panic!(
                        "sgpt did not exit before deadline; status: {:?}, stdout: {}, stderr: {}",
                        output.status,
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                Err(err) => panic!("failed to wait for sgpt: {err}"),
            }
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn accept_with_deadline(listener: &TcpListener) -> Result<TcpStream, String> {
    listener
        .set_nonblocking(true)
        .map_err(|err| err.to_string())?;
    let deadline = Instant::now() + DEADLINE;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .map_err(|err| err.to_string())?;
                return Ok(stream);
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                return Err("provider was not contacted before deadline".into());
            }
            Err(err) => return Err(format!("failed to accept provider connection: {err}")),
        }
    }
}

fn read_complete_http_request(stream: &mut TcpStream) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .map_err(|err| err.to_string())?;
    let deadline = Instant::now() + DEADLINE;
    let mut request = Vec::new();
    let mut buffer = [0_u8; 8192];

    let header_end = loop {
        if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        if request.len() > 64 * 1024 {
            return Err("provider request headers exceeded 64 KiB".into());
        }
        match stream.read(&mut buffer) {
            Ok(0) => return Err("provider connection closed before request headers".into()),
            Ok(read) => request.extend_from_slice(&buffer[..read]),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) && Instant::now() < deadline => {}
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err("provider request headers timed out".into());
            }
            Err(err) => return Err(format!("failed to read provider request headers: {err}")),
        }
    };

    let headers = std::str::from_utf8(&request[..header_end])
        .map_err(|err| format!("provider request headers were not UTF-8: {err}"))?;
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim())
        })
        .ok_or_else(|| "provider request omitted Content-Length".to_string())?
        .parse::<usize>()
        .map_err(|err| format!("invalid provider Content-Length: {err}"))?;
    let total = header_end
        .checked_add(content_length)
        .ok_or_else(|| "provider request length overflowed".to_string())?;
    if total > 2 * 1024 * 1024 {
        return Err("provider request exceeded 2 MiB".into());
    }

    while request.len() < total {
        match stream.read(&mut buffer) {
            Ok(0) => return Err("provider connection closed before request body".into()),
            Ok(read) => request.extend_from_slice(&buffer[..read]),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) && Instant::now() < deadline => {}
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err("provider request body timed out".into());
            }
            Err(err) => return Err(format!("failed to read provider request body: {err}")),
        }
    }
    Ok(())
}

fn wait_for_peer_close(stream: &mut TcpStream) -> Result<(), String> {
    let deadline = Instant::now() + DEADLINE;
    let mut buffer = [0_u8; 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                        | io::ErrorKind::BrokenPipe
                ) =>
            {
                return Ok(());
            }
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) && Instant::now() < deadline => {}
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err("provider connection remained open after SIGINT".into());
            }
            Err(err) => return Err(format!("failed while waiting for provider close: {err}")),
        }
    }
}

struct HangingProvider {
    endpoint: String,
    received: Receiver<Result<(), String>>,
    handle: JoinHandle<Result<(), String>>,
}

fn hanging_provider() -> HangingProvider {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
    let address = listener.local_addr().unwrap();
    let (received_tx, received_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut stream = match accept_with_deadline(&listener) {
            Ok(stream) => stream,
            Err(err) => {
                let _ = received_tx.send(Err(err.clone()));
                return Err(err);
            }
        };
        if let Err(err) = read_complete_http_request(&mut stream) {
            let _ = received_tx.send(Err(err.clone()));
            return Err(err);
        }
        received_tx
            .send(Ok(()))
            .map_err(|_| "test stopped waiting for provider request".to_string())?;
        wait_for_peer_close(&mut stream)
    });
    HangingProvider {
        endpoint: format!("http://{address}"),
        received: received_rx,
        handle: server,
    }
}

fn join_with_deadline<T>(handle: JoinHandle<T>, timeout: Duration) -> T {
    let deadline = Instant::now() + timeout;
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(handle.is_finished(), "provider thread did not finish");
    handle.join().unwrap()
}

fn lock_dir(xdg: &Path) -> Option<PathBuf> {
    let local = xdg.join("sgpt").join("local");
    let entries = std::fs::read_dir(local).ok()?;
    for entry in entries.flatten() {
        let lock = entry.path().join("lock");
        if lock.is_dir() {
            return Some(lock);
        }
    }
    None
}

#[test]
fn ctrl_c_releases_the_session_lock_and_exits_130() {
    let temp = tempfile::tempdir().unwrap();
    // local_runtime_base only honors XDG_RUNTIME_DIR when it is private.
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let provider = hanging_provider();
    let mut command = Command::new(env!("CARGO_BIN_EXE_sgpt"));
    command
        .arg("hello")
        .env("XDG_RUNTIME_DIR", temp.path())
        .env("SGPT_BASE_URL", &provider.endpoint)
        .env("SGPT_API_KEY", "key")
        .env("SGPT_MODEL", "model")
        .env("SGPT_TIMEOUT_SECONDS", "30")
        .env("SGPT_MAX_PROJECTED_SESSIONS", "64")
        .env("SGPT_MAX_CONCURRENT_REQUESTS", "4")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in [
        "SGPT_PROXY",
        "SGPT_SYSTEM_PROMPT",
        "SGPT_DEBUG",
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        command.env_remove(name);
    }
    let child = ChildGuard::new(command.spawn().unwrap());

    match provider.received.recv_timeout(DEADLINE) {
        Ok(Ok(())) => {}
        Ok(Err(err)) => panic!("provider did not receive a complete request: {err}"),
        Err(err) => panic!("timed out waiting for provider request: {err}"),
    }
    let lock = lock_dir(temp.path()).expect("session lock must exist while request is in flight");
    let session_dir = lock.parent().unwrap().to_path_buf();

    let status = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "failed to send SIGINT to sgpt");

    let output = child.wait_with_output(DEADLINE);
    let provider_result = join_with_deadline(provider.handle, DEADLINE);
    assert!(
        provider_result.is_ok(),
        "provider error: {provider_result:?}"
    );
    assert_eq!(output.status.code(), Some(130));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("interrupted"), "stderr: {stderr}");
    assert!(
        !stderr.contains("failed to remove session lock"),
        "stderr: {stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "interrupted request printed an answer"
    );
    assert!(!lock.exists(), "stale lock left behind at {lock:?}");
    assert!(!session_dir.join("current").exists());
    assert_eq!(
        std::fs::read_dir(session_dir.join("conversations"))
            .unwrap()
            .count(),
        0,
        "interrupted request persisted conversation history"
    );
}
