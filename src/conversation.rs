use anyhow::{bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

pub const LOGICAL_HIGH_WATER: usize = 2 * 1024 * 1024;
pub const LOGICAL_LOW_WATER: usize = 1536 * 1024;
pub const ANCHOR_LIMIT: usize = 10;
pub const ANCHOR_BUDGET: usize = 150 * 1024;
const TRUNCATION_MARKER: &str = "\n\n[history truncated]\n\n";

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct UserInput {
    pub instruction: String,
    pub stdin: String,
    pub timestamp: String,
}

impl UserInput {
    pub fn new(instruction: impl Into<String>, stdin: impl Into<String>) -> Self {
        Self {
            instruction: instruction.into(),
            stdin: stdin.into(),
            timestamp: timestamp(),
        }
    }

    pub fn rendered(&self) -> String {
        match (self.instruction.is_empty(), self.stdin.is_empty()) {
            (false, false) => format!("{}\n\nInput:\n{}", self.instruction, self.stdin),
            (false, true) => self.instruction.clone(),
            (true, false) => self.stdin.clone(),
            (true, true) => String::new(),
        }
    }

    pub fn logical_bytes(&self) -> usize {
        self.instruction.len() + self.stdin.len()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct AssistantResponse {
    pub content: String,
    pub timestamp: String,
}

impl AssistantResponse {
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            timestamp: timestamp(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Turn {
    pub request_id: String,
    pub request_digest: String,
    pub user: UserInput,
    pub assistant: AssistantResponse,
}

impl Turn {
    pub fn logical_bytes(&self) -> usize {
        self.user.logical_bytes() + self.assistant.content.len()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct InputAnchor {
    pub instruction: String,
    pub stdin: String,
    pub timestamp: String,
}

impl From<&UserInput> for InputAnchor {
    fn from(input: &UserInput) -> Self {
        Self {
            instruction: input.instruction.clone(),
            stdin: input.stdin.clone(),
            timestamp: input.timestamp.clone(),
        }
    }
}

impl InputAnchor {
    pub fn rendered(&self) -> String {
        UserInput {
            instruction: self.instruction.clone(),
            stdin: self.stdin.clone(),
            timestamp: self.timestamp.clone(),
        }
        .rendered()
    }

    fn logical_bytes(&self) -> usize {
        self.instruction.len() + self.stdin.len()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Conversation {
    turns: Vec<Turn>,
    anchors: Vec<InputAnchor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitResult {
    Committed,
    Replayed(String),
}

impl Conversation {
    pub fn from_parts(turns: Vec<Turn>, anchors: Vec<InputAnchor>) -> anyhow::Result<Self> {
        let mut ids = HashSet::new();
        for turn in &turns {
            ensure!(!turn.request_id.is_empty(), "turn request id is empty");
            ensure!(ids.insert(turn.request_id.clone()), "duplicate request id");
        }
        let anchors = select_anchors(&anchors);
        debug_assert!(anchors_obey_invariant(&anchors));
        Ok(Self { turns, anchors })
    }

    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    pub fn anchors(&self) -> &[InputAnchor] {
        &self.anchors
    }

    pub fn answer_for(&self, request_id: &str, digest: &str) -> anyhow::Result<Option<String>> {
        let Some(turn) = self.turns.iter().find(|turn| turn.request_id == request_id) else {
            return Ok(None);
        };
        if turn.request_digest != digest {
            bail!("request_id was already used with different input.");
        }
        Ok(Some(turn.assistant.content.clone()))
    }

    pub fn commit(&mut self, turn: Turn) -> anyhow::Result<CommitResult> {
        if let Some(answer) = self.answer_for(&turn.request_id, &turn.request_digest)? {
            return Ok(CommitResult::Replayed(answer));
        }
        ensure!(
            turn.logical_bytes() <= LOGICAL_HIGH_WATER,
            "new conversation turn exceeded 2 MiB history limit."
        );
        self.turns.push(turn);
        self.compact();
        debug_assert!(anchors_obey_invariant(&self.anchors));
        Ok(CommitResult::Committed)
    }

    pub fn logical_bytes(&self) -> usize {
        self.turns.iter().map(Turn::logical_bytes).sum::<usize>()
            + self
                .anchors
                .iter()
                .map(InputAnchor::logical_bytes)
                .sum::<usize>()
    }

    pub fn compact_oldest_turn_for_external_limit(&mut self) -> bool {
        if self.turns.len() <= 1 {
            return false;
        }
        let removed = self.turns.remove(0);
        if !removed.user.stdin.is_empty() {
            self.anchors.push(InputAnchor::from(&removed.user));
        }
        self.anchors = select_anchors(&self.anchors);
        debug_assert!(anchors_obey_invariant(&self.anchors));
        true
    }

    fn compact(&mut self) {
        if self.logical_bytes() <= LOGICAL_HIGH_WATER {
            return;
        }
        while self.turns.len() > 1 && self.logical_bytes() > LOGICAL_LOW_WATER {
            let removed = self.turns.remove(0);
            if !removed.user.stdin.is_empty() {
                self.anchors.push(InputAnchor::from(&removed.user));
            }
        }
        self.compact_anchors_to_fit();
        debug_assert!(anchors_obey_invariant(&self.anchors));
    }

    fn compact_anchors_to_fit(&mut self) {
        if self.logical_bytes() <= LOGICAL_LOW_WATER {
            return;
        }
        self.anchors = select_anchors(&self.anchors);
        while self.anchors.len() > 1 && self.logical_bytes() > LOGICAL_LOW_WATER {
            self.anchors.remove(1);
        }
        if self.logical_bytes() > LOGICAL_LOW_WATER && !self.anchors.is_empty() {
            let available = LOGICAL_LOW_WATER
                .saturating_sub(self.turns.iter().map(Turn::logical_bytes).sum::<usize>());
            self.anchors[0] = truncate_anchor(&self.anchors[0], available);
        }
    }
}

pub fn request_digest(mode: &str, input: &UserInput, context_json: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(mode.as_bytes());
    hasher.update([0]);
    hasher.update(input.instruction.as_bytes());
    hasher.update([0]);
    hasher.update(input.stdin.as_bytes());
    hasher.update([0]);
    hasher.update(context_json);
    hex::encode(hasher.finalize())
}

fn anchors_obey_invariant(anchors: &[InputAnchor]) -> bool {
    anchors.len() <= ANCHOR_LIMIT
        && anchors
            .iter()
            .map(InputAnchor::logical_bytes)
            .sum::<usize>()
            <= ANCHOR_BUDGET
}

fn select_anchors(anchors: &[InputAnchor]) -> Vec<InputAnchor> {
    if anchors.is_empty() {
        return Vec::new();
    }
    if anchors.len() <= ANCHOR_LIMIT
        && anchors
            .iter()
            .map(InputAnchor::logical_bytes)
            .sum::<usize>()
            <= ANCHOR_BUDGET
    {
        return anchors.to_vec();
    }
    let first_budget = ANCHOR_BUDGET / 2;
    let first = truncate_anchor(&anchors[0], first_budget);
    let mut selected = vec![first];
    let mut used = selected[0].logical_bytes();
    for anchor in anchors.iter().skip(1).rev() {
        if selected.len() >= ANCHOR_LIMIT {
            break;
        }
        let remaining = ANCHOR_BUDGET.saturating_sub(used);
        if remaining == 0 {
            break;
        }
        let anchor = truncate_anchor(anchor, remaining);
        let size = anchor.logical_bytes();
        if size == 0 {
            continue;
        }
        used += size;
        selected.push(anchor);
    }
    if selected.len() > 1 {
        selected[1..].reverse();
    }
    selected
}

fn truncate_anchor(anchor: &InputAnchor, max_bytes: usize) -> InputAnchor {
    if anchor.logical_bytes() <= max_bytes {
        return anchor.clone();
    }
    let rendered = anchor.rendered();
    let truncated = truncate_utf8_middle(&rendered, max_bytes);
    InputAnchor {
        instruction: String::new(),
        stdin: truncated,
        timestamp: anchor.timestamp.clone(),
    }
}

fn truncate_utf8_middle(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    if max_bytes <= TRUNCATION_MARKER.len() {
        return utf8_prefix(value, max_bytes).to_string();
    }
    let keep = max_bytes - TRUNCATION_MARKER.len();
    let head_len = keep / 2;
    let tail_len = keep - head_len;
    let head = utf8_prefix(value, head_len);
    let tail = utf8_suffix(value, tail_len);
    format!("{head}{TRUNCATION_MARKER}{tail}")
}

fn utf8_prefix(value: &str, max: usize) -> &str {
    let mut end = max.min(value.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn utf8_suffix(value: &str, max: usize) -> &str {
    let mut start = value.len().saturating_sub(max);
    while !value.is_char_boundary(start) {
        start += 1;
    }
    &value[start..]
}

pub fn timestamp() -> String {
    let output = std::process::Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output();
    output
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(id: usize, stdin: &str, assistant_size: usize) -> Turn {
        Turn {
            request_id: format!("{id:016x}"),
            request_digest: format!("digest-{id}"),
            user: UserInput {
                instruction: format!("instruction-{id}"),
                stdin: stdin.into(),
                timestamp: format!("time-{id}"),
            },
            assistant: AssistantResponse {
                content: "a".repeat(assistant_size),
                timestamp: format!("answer-{id}"),
            },
        }
    }

    #[test]
    fn compact_keeps_newest_turn_and_complete_turns() {
        let mut conversation = Conversation::default();
        for id in 0..7 {
            conversation
                .commit(turn(id, if id == 0 { "old stdin" } else { "" }, 300_000))
                .unwrap();
        }
        assert!(conversation.logical_bytes() <= LOGICAL_LOW_WATER);
        assert_eq!(
            conversation.turns().last().unwrap().request_id,
            "0000000000000006"
        );
        assert!(
            conversation
                .anchors()
                .iter()
                .any(|anchor| anchor.stdin == "old stdin")
        );
    }

    #[test]
    fn idempotency_replays_same_digest_and_rejects_conflict() {
        let mut conversation = Conversation::default();
        conversation.commit(turn(1, "", 1)).unwrap();
        assert!(matches!(
            conversation.commit(turn(1, "", 1)).unwrap(),
            CommitResult::Replayed(_)
        ));
        let mut conflict = turn(1, "", 1);
        conflict.request_digest = "different".into();
        assert!(conversation.commit(conflict).is_err());
        assert_eq!(conversation.turns().len(), 1);
    }

    #[test]
    fn oversized_anchor_is_utf8_safe_and_keeps_ends() {
        let anchor = InputAnchor {
            instruction: "inspect".into(),
            stdin: format!("头{}尾", "界".repeat(100_000)),
            timestamp: "t".into(),
        };
        let selected = select_anchors(&[anchor]);
        let value = selected[0].rendered();
        assert!(value.len() <= ANCHOR_BUDGET / 2);
        assert!(value.contains("[history truncated]"));
        assert!(value.contains('头'));
        assert!(value.contains('尾'));
    }

    fn anchor(id: usize, size: usize) -> InputAnchor {
        InputAnchor {
            instruction: format!("anchor-{id}"),
            stdin: "x".repeat(size),
            timestamp: format!("time-{id}"),
        }
    }

    #[test]
    fn anchor_count_keeps_first_and_most_recent_nine() {
        let anchors = (0..11).map(|id| anchor(id, 1)).collect::<Vec<_>>();
        let selected = select_anchors(&anchors);
        assert_eq!(selected.len(), ANCHOR_LIMIT);
        assert_eq!(selected[0].timestamp, "time-0");
        assert_eq!(selected[1].timestamp, "time-2");
        assert_eq!(selected.last().unwrap().timestamp, "time-10");

        let unchanged = select_anchors(&anchors[..ANCHOR_LIMIT]);
        assert_eq!(unchanged, anchors[..ANCHOR_LIMIT]);
    }

    #[test]
    fn from_parts_deterministically_normalizes_historical_anchors() {
        let anchors = (0..20)
            .map(|id| anchor(id, ANCHOR_BUDGET))
            .collect::<Vec<_>>();
        let conversation = Conversation::from_parts(Vec::new(), anchors).unwrap();

        assert!(anchors_obey_invariant(conversation.anchors()));
        assert_eq!(conversation.anchors()[0].timestamp, "time-0");
        assert_eq!(conversation.anchors().last().unwrap().timestamp, "time-19");
        assert!(
            conversation.anchors()[0]
                .rendered()
                .contains("[history truncated]")
        );
    }
}
