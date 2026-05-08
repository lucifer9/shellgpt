use crate::ai::OpenAiClient;
use crate::context::ContextBlock;
use crate::error::ERR_NO_PREVIOUS;
use crate::ids;
use crate::input::UserInput;
use anyhow::{Context as _, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::process::Command;
use tokio::time::{Duration, timeout};

pub const HISTORY_LOAD_LIMIT: u64 = 2 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct LocalSession {
    dir: PathBuf,
    session_id: String,
}

impl LocalSession {
    pub async fn resolve() -> anyhow::Result<Self> {
        let key = local_session_key().await?;
        let base = local_runtime_base()?;
        let dir = base.join("local").join(&key);
        secure_dir(&dir)?;
        secure_dir(&dir.join("conversations"))?;
        Ok(Self {
            dir,
            session_id: ids::id128()?,
        })
    }

    #[cfg(test)]
    pub fn at_for_tests(dir: PathBuf) -> Self {
        Self {
            dir,
            session_id: "0123456789abcdef".into(),
        }
    }

    fn conversations_dir(&self) -> PathBuf {
        self.dir.join("conversations")
    }

    fn current_path(&self) -> PathBuf {
        self.dir.join("current")
    }

    fn conversation_path(&self, id: &str) -> anyhow::Result<PathBuf> {
        ensure!(ids::is_hex_id(id), "invalid conversation id");
        Ok(self.conversations_dir().join(format!("{id}.jsonl")))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct HistoryEntry {
    pub role: String,
    pub content: String,
    pub ts: String,
}

impl HistoryEntry {
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            ts: utc_timestamp(),
        }
    }
}

pub async fn run_local_request(
    session: &LocalSession,
    client: &OpenAiClient,
    config: &crate::config::AiConfig,
    context: &ContextBlock,
    continue_mode: bool,
    input: UserInput,
) -> anyhow::Result<()> {
    let request_id = ids::id128()?;
    let _lock = SessionLock::acquire(
        session,
        &request_id,
        if continue_mode { "continue" } else { "normal" },
    )?;

    let (conversation_id, history) = if continue_mode {
        let current = read_current(session)?.ok_or_else(|| anyhow::anyhow!(ERR_NO_PREVIOUS))?;
        let history = load_history(session, &current)?;
        (current, history)
    } else {
        (ids::id128()?, Vec::new())
    };

    let answer = client.ask(context, &history, &input).await?;
    let pair = [
        HistoryEntry::new("user", input.prompt),
        HistoryEntry::new("assistant", answer.clone()),
    ];
    append_pair(session, &conversation_id, &pair)?;
    if !continue_mode {
        write_current(session, &conversation_id)?;
    }
    drop(_lock);
    print_answer(&answer)?;
    if config.debug {
        crate::debug::log("session_id", &session.session_id);
        crate::debug::log("session_dir", session.dir.display());
    }
    Ok(())
}

fn print_answer(answer: &str) -> anyhow::Result<()> {
    print!("{answer}");
    if !answer.ends_with('\n') {
        println!();
    }
    std::io::stdout().flush()?;
    Ok(())
}

pub fn load_history(
    session: &LocalSession,
    conversation_id: &str,
) -> anyhow::Result<Vec<HistoryEntry>> {
    let path = session.conversation_path(conversation_id)?;
    let metadata =
        fs::metadata(&path).with_context(|| format!("failed to read {}", path.display()))?;
    ensure!(
        metadata.len() <= HISTORY_LOAD_LIMIT,
        "conversation history exceeded 2 MiB limit."
    );
    let mut text = String::new();
    fs::File::open(&path)?.read_to_string(&mut text)?;
    parse_jsonl(&text)
}

pub fn parse_jsonl(text: &str) -> anyhow::Result<Vec<HistoryEntry>> {
    let mut entries = Vec::new();
    for (line_no, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let entry: HistoryEntry = serde_json::from_str(line)
            .with_context(|| format!("invalid history JSONL at line {}", line_no + 1))?;
        ensure!(
            matches!(entry.role.as_str(), "user" | "assistant"),
            "invalid history role at line {}",
            line_no + 1
        );
        entries.push(entry);
    }
    Ok(entries)
}

pub fn append_pair(
    session: &LocalSession,
    conversation_id: &str,
    pair: &[HistoryEntry; 2],
) -> anyhow::Result<()> {
    let path = session.conversation_path(conversation_id)?;
    let mut pair_text = String::new();
    for entry in pair {
        pair_text.push_str(&serde_json::to_string(entry)?);
        pair_text.push('\n');
    }
    if pair_text.len() as u64 > HISTORY_LOAD_LIMIT {
        bail!("new conversation turn exceeded 2 MiB history limit.");
    }
    let existing_len = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if existing_len + pair_text.len() as u64 <= HISTORY_LOAD_LIMIT {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(pair_text.as_bytes())?;
        set_file_0600(&path)?;
        return Ok(());
    }
    compact_and_write(session, conversation_id, pair)
}

fn compact_and_write(
    session: &LocalSession,
    conversation_id: &str,
    pair: &[HistoryEntry; 2],
) -> anyhow::Result<()> {
    let mut entries = load_history(session, conversation_id).unwrap_or_default();
    entries.push(pair[0].clone());
    entries.push(pair[1].clone());

    let mut kept_rev = Vec::new();
    let mut i = entries.len();
    while i >= 2 {
        let user = &entries[i - 2];
        let assistant = &entries[i - 1];
        if user.role == "user" && assistant.role == "assistant" {
            kept_rev.push(assistant.clone());
            kept_rev.push(user.clone());
        }
        i -= 2;
    }
    kept_rev.reverse();

    let mut text = String::new();
    for entry in kept_rev {
        let line = serde_json::to_string(&entry)?;
        if (text.len() + line.len() + 1) as u64 > HISTORY_LOAD_LIMIT {
            if text.is_empty() {
                bail!("new conversation turn exceeded 2 MiB history limit.");
            }
            break;
        }
        text.push_str(&line);
        text.push('\n');
    }
    let path = session.conversation_path(conversation_id)?;
    atomic_write_0600(&path, text.as_bytes())
}

fn read_current(session: &LocalSession) -> anyhow::Result<Option<String>> {
    let path = session.current_path();
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)?;
    let id = text.trim_end_matches('\n');
    ensure!(ids::is_hex_id(id), "current conversation id is invalid");
    Ok(Some(id.to_string()))
}

fn write_current(session: &LocalSession, conversation_id: &str) -> anyhow::Result<()> {
    ensure!(ids::is_hex_id(conversation_id), "invalid conversation id");
    atomic_write_0600(
        &session.current_path(),
        format!("{conversation_id}\n").as_bytes(),
    )
}

fn atomic_write_0600(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let parent = path.parent().context("path has no parent")?;
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        path.file_name().unwrap().to_string_lossy(),
        ids::id128()?
    ));
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
    }
    fs::rename(&tmp, path)?;
    set_file_0600(path)?;
    Ok(())
}

#[derive(Debug)]
struct SessionLock {
    path: PathBuf,
}

impl SessionLock {
    fn acquire(session: &LocalSession, request_id: &str, mode: &str) -> anyhow::Result<Self> {
        let path = session.dir.join("lock");
        match fs::create_dir(&path) {
            Ok(()) => {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
                let metadata = format!(
                    "pid={}\nrequest_id={request_id}\ncreated_at={}\nmode={mode}\n",
                    std::process::id(),
                    utc_timestamp()
                );
                atomic_write_0600(&path.join("metadata"), metadata.as_bytes())?;
                Ok(Self { path })
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!(
                    "sgpt session is locked at {}. If you are sure it is stale, remove it with: rmdir {}",
                    path.display(),
                    path.display()
                )
            }
            Err(err) => Err(err.into()),
        }
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.path.join("metadata"));
        let _ = fs::remove_dir(&self.path);
    }
}

async fn local_session_key() -> anyhow::Result<String> {
    if let Some(tty) = controlling_tty().await {
        return Ok(format!("tty-{}", hash32(&tty)));
    }
    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    let canonical = cwd.canonicalize().unwrap_or(cwd);
    Ok(format!("cwd-{}", hash32(&canonical.display().to_string())))
}

async fn controlling_tty() -> Option<String> {
    let tty_file = fs::File::open("/dev/tty").ok()?;
    let stdin = Stdio::from(tty_file);
    let child = Command::new("tty")
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    match timeout(Duration::from_secs(1), child.wait_with_output()).await {
        Ok(Ok(output)) if output.status.success() => {
            Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
        _ => None,
    }
}

fn local_runtime_base() -> anyhow::Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        let path = PathBuf::from(xdg).join("sgpt");
        if usable_private_dir(path.parent().unwrap_or(Path::new("/"))) {
            secure_dir(&path)?;
            return Ok(path);
        }
    }
    let uid = uid()?;
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let path = PathBuf::from(tmp).join(format!("sgpt-{uid}"));
    secure_dir(&path)?;
    Ok(path)
}

fn usable_private_dir(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0
}

fn secure_dir(path: &Path) -> anyhow::Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn set_file_0600(path: &Path) -> anyhow::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn uid() -> anyhow::Result<String> {
    let output = std::process::Command::new("id")
        .arg("-u")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    ensure!(output.status.success(), "failed to determine uid");
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn hash32(value: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    hex::encode(hasher.finalize())[..32].to_string()
}

fn utc_timestamp() -> String {
    let output = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_jsonl_and_rejects_bad_schema() {
        let text = r#"{"role":"user","content":"hi","ts":"2026-05-07T10:00:00Z"}
{"role":"assistant","content":"hello","ts":"2026-05-07T10:00:01Z"}
"#;
        assert_eq!(parse_jsonl(text).unwrap().len(), 2);
        let bad = r#"{"role":"system","content":"x","ts":""}"#;
        assert!(parse_jsonl(bad).is_err());
    }

    #[test]
    fn appends_pair_and_updates_current_atomically() {
        let temp = tempfile::tempdir().unwrap();
        secure_dir(&temp.path().join("conversations")).unwrap();
        let session = LocalSession::at_for_tests(temp.path().to_path_buf());
        let id = "0123456789abcdef";
        let pair = [
            HistoryEntry::new("user", "hi"),
            HistoryEntry::new("assistant", "hello"),
        ];
        append_pair(&session, id, &pair).unwrap();
        write_current(&session, id).unwrap();
        assert_eq!(read_current(&session).unwrap().as_deref(), Some(id));
        assert_eq!(load_history(&session, id).unwrap().len(), 2);
        assert_eq!(
            fs::metadata(session.conversation_path(id).unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn lock_directory_prevents_second_acquisition() {
        let temp = tempfile::tempdir().unwrap();
        let session = LocalSession::at_for_tests(temp.path().to_path_buf());
        let lock = SessionLock::acquire(&session, "0123456789abcdef", "normal").unwrap();
        let err = SessionLock::acquire(&session, "0123456789abcdef", "normal")
            .unwrap_err()
            .to_string();
        assert!(err.contains("session is locked"));
        drop(lock);
        assert!(SessionLock::acquire(&session, "0123456789abcdef", "normal").is_ok());
    }
}
