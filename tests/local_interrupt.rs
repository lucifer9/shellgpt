use std::io::{self, Read};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

mod common;
use common::{private_runtime_dir, read_http_request};

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

/// Returns once the peer closes the connection, or fails after DEADLINE.
fn wait_for_peer_close(stream: &mut TcpStream) {
    let mut buffer = [0_u8; 1024];
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => return,
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::ConnectionReset => return,
            Err(err) => panic!("provider connection did not close after SIGINT: {err}"),
        }
    }
}

struct HangingProvider {
    endpoint: String,
    received: Receiver<()>,
    handle: JoinHandle<()>,
}

/// Accepts one request and never answers it. A panic in the server thread drops
/// `received`'s sender, so the test thread fails fast instead of hanging.
fn hanging_provider() -> HangingProvider {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
    let address = listener.local_addr().unwrap();
    let (received_tx, received_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_read_timeout(Some(DEADLINE)).unwrap();
        read_http_request(&mut stream);
        received_tx.send(()).unwrap();
        wait_for_peer_close(&mut stream);
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
    let runtime = private_runtime_dir(temp.path());
    let provider = hanging_provider();
    let mut command = Command::new(env!("CARGO_BIN_EXE_sgpt"));
    command
        .arg("hello")
        .env("XDG_RUNTIME_DIR", &runtime)
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

    provider
        .received
        .recv_timeout(DEADLINE)
        .expect("provider must receive a complete request");
    let lock = lock_dir(&runtime).expect("session lock must exist while request is in flight");
    let session_dir = lock.parent().unwrap().to_path_buf();

    let status = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "failed to send SIGINT to sgpt");

    let output = child.wait_with_output(DEADLINE);
    join_with_deadline(provider.handle, DEADLINE);
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
