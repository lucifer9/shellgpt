use crate::conversation::{
    AssistantResponse, Conversation, InputAnchor, Turn, UserInput, request_digest,
};
use crate::ids;
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

pub const HISTORY_PHYSICAL_LIMIT: u64 = 16 * 1024 * 1024;
const HISTORY_PHYSICAL_TARGET: u64 = 14 * 1024 * 1024;

#[derive(Clone, Debug)]
pub(super) struct LocalSession {
    dir: PathBuf,
    session_id: String,
}

impl LocalSession {
    pub(super) async fn resolve() -> anyhow::Result<Self> {
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

    pub(super) fn session_id(&self) -> &str {
        &self.session_id
    }
    pub(super) fn dir(&self) -> &Path {
        &self.dir
    }
    fn conversations_dir(&self) -> PathBuf {
        self.dir.join("conversations")
    }
    pub(super) fn current_path(&self) -> PathBuf {
        self.dir.join("current")
    }
    pub(super) fn conversation_path(&self, id: &str) -> anyhow::Result<PathBuf> {
        ensure!(ids::is_hex_id(id), "invalid conversation id");
        Ok(self.conversations_dir().join(format!("{id}.jsonl")))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct V2Record {
    version: u8,
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    turn: Option<Turn>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_anchor: Option<InputAnchor>,
}

#[derive(Clone, Debug, Deserialize)]
struct V1Entry {
    role: String,
    content: String,
    ts: String,
    #[serde(default)]
    stdin_bytes: Option<usize>,
}

pub(super) fn load_conversation(
    session: &LocalSession,
    conversation_id: &str,
) -> anyhow::Result<Conversation> {
    let path = session.conversation_path(conversation_id)?;
    let metadata =
        fs::metadata(&path).with_context(|| format!("failed to read {}", path.display()))?;
    ensure!(
        metadata.len() <= HISTORY_PHYSICAL_LIMIT,
        "conversation history exceeded 16 MiB physical limit."
    );
    let mut text = String::new();
    fs::File::open(&path)?.read_to_string(&mut text)?;
    parse_history(&text)
}

fn parse_history(text: &str) -> anyhow::Result<Conversation> {
    let lines = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    if lines.is_empty() {
        return Ok(Conversation::default());
    }
    let first: serde_json::Value =
        serde_json::from_str(lines[0]).context("invalid history JSONL at line 1")?;
    if first.get("version").and_then(|value| value.as_u64()) == Some(2) {
        parse_v2(&lines)
    } else {
        migrate_v1_in_memory(&lines)
    }
}

fn parse_v2(lines: &[&str]) -> anyhow::Result<Conversation> {
    let mut turns = Vec::new();
    let mut anchors = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let record: V2Record = serde_json::from_str(line)
            .with_context(|| format!("invalid history JSONL at line {}", index + 1))?;
        ensure!(
            record.version == 2,
            "unsupported history version at line {}",
            index + 1
        );
        match record.kind.as_str() {
            "turn" => turns.push(record.turn.context("turn record is missing turn")?),
            "input_anchor" => anchors.push(
                record
                    .input_anchor
                    .context("anchor record is missing input_anchor")?,
            ),
            _ => bail!("invalid history kind at line {}", index + 1),
        }
    }
    Conversation::from_parts(turns, anchors)
}

fn migrate_v1_in_memory(lines: &[&str]) -> anyhow::Result<Conversation> {
    ensure!(
        lines.len().is_multiple_of(2),
        "version 1 history contains an incomplete Turn"
    );
    let mut turns = Vec::new();
    for (pair_index, pair) in lines.chunks(2).enumerate() {
        let user: V1Entry = serde_json::from_str(pair[0])
            .with_context(|| format!("invalid version 1 history at line {}", pair_index * 2 + 1))?;
        let assistant: V1Entry = serde_json::from_str(pair[1])
            .with_context(|| format!("invalid version 1 history at line {}", pair_index * 2 + 2))?;
        ensure!(
            user.role == "user" && assistant.role == "assistant",
            "version 1 history is not adjacent user + assistant Turns"
        );
        let (instruction, stdin) = split_v1_input(&user.content, user.stdin_bytes.unwrap_or(0))?;
        let request_id = format!("{:016x}", pair_index + 1);
        let input = UserInput {
            instruction,
            stdin,
            timestamp: user.ts,
        };
        let digest = request_digest("migrated-v1", &input, &[]);
        turns.push(Turn {
            request_id,
            request_digest: digest,
            user: input,
            assistant: AssistantResponse {
                content: assistant.content,
                timestamp: assistant.ts,
            },
        });
    }
    Conversation::from_parts(turns, Vec::new())
}

fn split_v1_input(content: &str, stdin_bytes: usize) -> anyhow::Result<(String, String)> {
    if stdin_bytes == 0 {
        return Ok((content.to_string(), String::new()));
    }
    ensure!(
        stdin_bytes <= content.len(),
        "version 1 stdin_bytes exceeds user content"
    );
    let split = content.len() - stdin_bytes;
    ensure!(
        content.is_char_boundary(split),
        "version 1 stdin_bytes splits UTF-8"
    );
    let stdin = content[split..].to_string();
    let prefix = &content[..split];
    if prefix.is_empty() {
        return Ok((String::new(), stdin));
    }
    let instruction = prefix
        .strip_suffix("\n\nInput:\n")
        .context("version 1 stdin metadata cannot be split reliably")?;
    Ok((instruction.to_string(), stdin))
}

pub(super) fn save_conversation(
    session: &LocalSession,
    conversation_id: &str,
    before: &Conversation,
    after: &Conversation,
) -> anyhow::Result<()> {
    save_conversation_with_limits(
        session,
        conversation_id,
        before,
        after,
        HISTORY_PHYSICAL_TARGET,
        HISTORY_PHYSICAL_LIMIT,
    )
}

fn save_conversation_with_limits(
    session: &LocalSession,
    conversation_id: &str,
    before: &Conversation,
    after: &Conversation,
    target: u64,
    limit: u64,
) -> anyhow::Result<()> {
    let path = session.conversation_path(conversation_id)?;
    let existing_matches_before = if path.exists() {
        fs::read(&path)? == serialize_v2(before)?
    } else {
        before.turns().is_empty() && before.anchors().is_empty()
    };
    let (persisted, bytes, physically_compacted) =
        prepare_for_physical_limit(after, target as usize, limit as usize)?;
    let append_fast_path = !physically_compacted
        && existing_matches_before
        && after.anchors() == before.anchors()
        && after.turns().len() == before.turns().len() + 1;
    if append_fast_path {
        let record = V2Record {
            version: 2,
            kind: "turn".into(),
            turn: after.turns().last().cloned(),
            input_anchor: None,
        };
        let mut line = serde_json::to_vec(&record)?;
        line.push(b'\n');
        let existing = fs::metadata(&path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        ensure!(
            existing + line.len() as u64 <= limit,
            "conversation history exceeded 16 MiB physical limit."
        );
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(&line)?;
        set_file_0600(&path)?;
        return Ok(());
    }
    debug_assert_eq!(bytes, serialize_v2(&persisted)?);
    atomic_write_0600(&path, &bytes)
}

fn prepare_for_physical_limit(
    conversation: &Conversation,
    target: usize,
    limit: usize,
) -> anyhow::Result<(Conversation, Vec<u8>, bool)> {
    let mut persisted = conversation.clone();
    let mut bytes = serialize_v2(&persisted)?;
    let mut compacted = false;
    while bytes.len() > target {
        if !persisted.compact_oldest_turn_for_external_limit() {
            break;
        }
        compacted = true;
        bytes = serialize_v2(&persisted)?;
    }
    ensure!(
        bytes.len() <= limit,
        "conversation history exceeded 16 MiB physical limit."
    );
    Ok((persisted, bytes, compacted))
}

fn serialize_v2(conversation: &Conversation) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for anchor in conversation.anchors() {
        serde_json::to_writer(
            &mut bytes,
            &V2Record {
                version: 2,
                kind: "input_anchor".into(),
                turn: None,
                input_anchor: Some(anchor.clone()),
            },
        )?;
        bytes.push(b'\n');
    }
    for turn in conversation.turns() {
        serde_json::to_writer(
            &mut bytes,
            &V2Record {
                version: 2,
                kind: "turn".into(),
                turn: Some(turn.clone()),
                input_anchor: None,
            },
        )?;
        bytes.push(b'\n');
    }
    Ok(bytes)
}

pub(super) fn read_current(session: &LocalSession) -> anyhow::Result<Option<String>> {
    let path = session.current_path();
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)?;
    let id = text.trim_end_matches('\n');
    ensure!(ids::is_hex_id(id), "current conversation id is invalid");
    Ok(Some(id.to_string()))
}

pub(super) fn write_current(session: &LocalSession, conversation_id: &str) -> anyhow::Result<()> {
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
    let result = (|| -> anyhow::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&tmp, path)?;
        set_file_0600(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[derive(Debug)]
pub(super) struct SessionLock {
    path: PathBuf,
}
impl SessionLock {
    pub(super) fn acquire(
        session: &LocalSession,
        request_id: &str,
        mode: &str,
    ) -> anyhow::Result<Self> {
        let path = session.dir.join("lock");
        match fs::create_dir(&path) {
            Ok(()) => {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
                let metadata = format!(
                    "pid={}\nrequest_id={request_id}\ncreated_at={}\nmode={mode}\n",
                    std::process::id(),
                    crate::conversation::timestamp()
                );
                atomic_write_0600(&path.join("metadata"), metadata.as_bytes())?;
                Ok(Self { path })
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => bail!(
                "sgpt session is locked at {}. If you are sure it is stale, remove it with: rmdir {}",
                path.display(),
                path.display()
            ),
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
    let child = Command::new("tty")
        .stdin(Stdio::from(tty_file))
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
    let path = PathBuf::from(std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into()))
        .join(format!("sgpt-{uid}"));
    secure_dir(&path)?;
    Ok(path)
}
fn usable_private_dir(path: &Path) -> bool {
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> (tempfile::TempDir, LocalSession) {
        let temp = tempfile::tempdir().unwrap();
        secure_dir(&temp.path().join("conversations")).unwrap();
        let session = LocalSession::at_for_tests(temp.path().to_path_buf());
        (temp, session)
    }

    fn short_turn(id: usize, stdin: &str) -> Turn {
        Turn {
            request_id: format!("{id:016x}"),
            request_digest: format!("digest-{id}"),
            user: UserInput {
                instruction: format!("instruction-{id}"),
                stdin: stdin.into(),
                timestamp: format!("user-{id}"),
            },
            assistant: AssistantResponse {
                content: "ok".into(),
                timestamp: format!("assistant-{id}"),
            },
        }
    }

    #[test]
    fn version_one_history_migrates_atomically() {
        let (_temp, session) = session();
        let id = "0123456789abcdef";
        let path = session.conversation_path(id).unwrap();
        fs::write(&path, "{\"role\":\"user\",\"content\":\"inspect\\n\\nInput:\\nhello\",\"ts\":\"u\",\"stdin_bytes\":5}\n{\"role\":\"assistant\",\"content\":\"ok\",\"ts\":\"a\"}\n").unwrap();
        let before = load_conversation(&session, id).unwrap();
        let mut after = before.clone();
        after
            .commit(Turn {
                request_id: "1111111111111111".into(),
                request_digest: "d".into(),
                user: UserInput::new("next", ""),
                assistant: AssistantResponse::new("answer"),
            })
            .unwrap();
        save_conversation(&session, id, &before, &after).unwrap();
        let text = fs::read_to_string(path).unwrap();
        assert!(text.lines().all(|line| line.contains("\"version\":2")));
        assert_eq!(load_conversation(&session, id).unwrap().turns().len(), 2);
    }

    #[test]
    fn corrupt_history_is_not_overwritten() {
        let (_temp, session) = session();
        let id = "0123456789abcdef";
        let path = session.conversation_path(id).unwrap();
        let original = b"{not-json}\n";
        fs::write(&path, original).unwrap();
        assert!(load_conversation(&session, id).is_err());
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn version_two_round_trip_and_permissions() {
        let (_temp, session) = session();
        let id = "0123456789abcdef";
        let before = Conversation::default();
        let mut after = before.clone();
        after
            .commit(Turn {
                request_id: "1111111111111111".into(),
                request_digest: "d".into(),
                user: UserInput::new("hi", ""),
                assistant: AssistantResponse::new("hello"),
            })
            .unwrap();
        save_conversation(&session, id, &before, &after).unwrap();
        assert_eq!(load_conversation(&session, id).unwrap().turns().len(), 1);
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
    fn oversized_v2_anchor_history_is_normalized_on_load_and_next_save() {
        let (_temp, session) = session();
        let id = "0123456789abcdef";
        let path = session.conversation_path(id).unwrap();
        let mut raw = Vec::new();
        for anchor_id in 0..11 {
            serde_json::to_writer(
                &mut raw,
                &V2Record {
                    version: 2,
                    kind: "input_anchor".into(),
                    turn: None,
                    input_anchor: Some(InputAnchor {
                        instruction: format!("anchor-{anchor_id}"),
                        stdin: "x".into(),
                        timestamp: format!("time-{anchor_id}"),
                    }),
                },
            )
            .unwrap();
            raw.push(b'\n');
        }
        fs::write(&path, &raw).unwrap();

        let before = load_conversation(&session, id).unwrap();
        assert_eq!(before.anchors().len(), crate::conversation::ANCHOR_LIMIT);
        assert_eq!(before.anchors()[0].timestamp, "time-0");
        assert_eq!(before.anchors()[1].timestamp, "time-2");
        assert_eq!(before.anchors().last().unwrap().timestamp, "time-10");

        let mut after = before.clone();
        after.commit(short_turn(100, "")).unwrap();
        save_conversation(&session, id, &before, &after).unwrap();

        let persisted = fs::read(&path).unwrap();
        assert_ne!(persisted, raw);
        assert_eq!(load_conversation(&session, id).unwrap(), after);
        assert_eq!(
            String::from_utf8(persisted)
                .unwrap()
                .lines()
                .filter(|line| line.contains("\"kind\":\"input_anchor\""))
                .count(),
            crate::conversation::ANCHOR_LIMIT
        );
    }

    #[test]
    fn lock_directory_prevents_second_acquisition() {
        let (_temp, session) = session();
        let lock = SessionLock::acquire(&session, "0123456789abcdef", "normal").unwrap();
        assert!(
            SessionLock::acquire(&session, "0123456789abcdef", "normal")
                .unwrap_err()
                .to_string()
                .contains("session is locked")
        );
        drop(lock);
        assert!(SessionLock::acquire(&session, "0123456789abcdef", "normal").is_ok());
    }

    #[test]
    fn physical_size_compaction_preserves_turn_boundaries_and_stdin_anchor() {
        let turns = (0..40)
            .map(|id| short_turn(id, if id == 0 { "important stdin" } else { "" }))
            .collect::<Vec<_>>();
        let conversation = Conversation::from_parts(turns, Vec::new()).unwrap();
        let (persisted, bytes, compacted) =
            prepare_for_physical_limit(&conversation, 2_500, 3_000).unwrap();
        assert!(compacted);
        assert!(bytes.len() <= 2_500);
        assert_eq!(
            persisted.turns().last().unwrap().request_id,
            "0000000000000027"
        );
        assert!(
            persisted
                .anchors()
                .iter()
                .any(|anchor| anchor.stdin == "important stdin")
        );
        assert!(persisted.anchors().len() <= crate::conversation::ANCHOR_LIMIT);
    }

    #[test]
    fn physical_compaction_rewrites_atomically_and_allows_future_append() {
        let (_temp, session) = session();
        let id = "0123456789abcdef";
        let before = Conversation::from_parts(
            (0..39).map(|turn_id| short_turn(turn_id, "")).collect(),
            Vec::new(),
        )
        .unwrap();
        let after = Conversation::from_parts(
            (0..40).map(|turn_id| short_turn(turn_id, "")).collect(),
            Vec::new(),
        )
        .unwrap();
        let path = session.conversation_path(id).unwrap();
        fs::write(&path, serialize_v2(&before).unwrap()).unwrap();
        save_conversation_with_limits(&session, id, &before, &after, 2_500, 3_000).unwrap();
        assert!(fs::metadata(&path).unwrap().len() <= 2_500);

        let persisted = load_conversation(&session, id).unwrap();
        let mut appended = persisted.clone();
        appended.commit(short_turn(100, "")).unwrap();
        save_conversation_with_limits(&session, id, &persisted, &appended, 2_500, 3_000).unwrap();
        assert_eq!(
            load_conversation(&session, id)
                .unwrap()
                .turns()
                .last()
                .unwrap()
                .request_id,
            "0000000000000064"
        );
    }

    #[test]
    fn oversized_latest_turn_leaves_history_and_current_unchanged() {
        let (_temp, session) = session();
        let id = "0123456789abcdef";
        let path = session.conversation_path(id).unwrap();
        let original = b"original history bytes\n";
        fs::write(&path, original).unwrap();
        fs::write(session.current_path(), "1111111111111111\n").unwrap();
        let current_before = fs::read(session.current_path()).unwrap();
        let after = Conversation::from_parts(
            vec![Turn {
                assistant: AssistantResponse::new("x".repeat(2_000)),
                ..short_turn(1, "")
            }],
            Vec::new(),
        )
        .unwrap();
        assert!(
            save_conversation_with_limits(
                &session,
                id,
                &Conversation::default(),
                &after,
                900,
                1_000,
            )
            .is_err()
        );
        assert_eq!(fs::read(path).unwrap(), original);
        assert_eq!(fs::read(session.current_path()).unwrap(), current_before);
    }
}
