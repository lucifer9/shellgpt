use super::ChatMessage;
use crate::history::HistoryEntry;

const HISTORY_MESSAGE_LIMIT: usize = 20;
const HISTORY_CHAR_LIMIT: usize = 200_000;
const HISTORY_ANCHOR_CHAR_LIMIT: usize = 150_000;
const HISTORY_TRUNCATION_MARKER: &str = "\n\n[history truncated]\n\n";

pub fn history_messages(entries: &[HistoryEntry]) -> Vec<ChatMessage> {
    let turns = paired_turns(entries);
    let mut selected: Vec<HistoryTurn<'_>> = Vec::new();
    let mut anchors = Vec::new();
    let mut chars = 0;
    let mut anchor_chars = 0;
    let mut messages = 0;

    for turn in &turns {
        if turn.user.stdin_bytes.unwrap_or(0) > 0 {
            try_select_anchor(
                &mut anchors,
                &mut chars,
                &mut anchor_chars,
                &mut messages,
                turn,
            );
        }
    }

    for turn in turns.iter().rev() {
        if let Some(anchor) = remove_anchor(
            &mut anchors,
            &mut chars,
            &mut anchor_chars,
            &mut messages,
            turn.index,
        ) {
            if !try_select_turn(&mut selected, &mut chars, &mut messages, turn) {
                restore_anchor(
                    &mut anchors,
                    &mut chars,
                    &mut anchor_chars,
                    &mut messages,
                    anchor,
                );
                break;
            }
        } else if !contains_turn(&selected, turn.index)
            && !try_select_turn(&mut selected, &mut chars, &mut messages, turn)
        {
            break;
        }
    }

    anchors.sort_by_key(|anchor| anchor.index);
    selected.sort_by_key(|turn| turn.index);
    let mut messages = anchors
        .into_iter()
        .map(|anchor| ChatMessage {
            role: anchor.role,
            content: anchor.content,
        })
        .collect::<Vec<_>>();
    messages.extend(selected.into_iter().flat_map(|turn| {
        [
            ChatMessage {
                role: turn.user.role.clone(),
                content: turn.user.content.clone(),
            },
            ChatMessage {
                role: turn.assistant.role.clone(),
                content: turn.assistant.content.clone(),
            },
        ]
    }));
    messages
}

fn paired_turns(entries: &[HistoryEntry]) -> Vec<HistoryTurn<'_>> {
    let mut turns = Vec::new();
    let mut i = 0;
    while i + 1 < entries.len() {
        if entries[i].role == "user" && entries[i + 1].role == "assistant" {
            turns.push(HistoryTurn {
                index: i,
                user: &entries[i],
                assistant: &entries[i + 1],
            });
            i += 2;
        } else {
            i += 1;
        }
    }
    turns
}

#[derive(Clone, Copy)]
struct HistoryTurn<'a> {
    index: usize,
    user: &'a HistoryEntry,
    assistant: &'a HistoryEntry,
}

struct AnchorMessage {
    index: usize,
    role: String,
    content: String,
    chars: usize,
}

fn try_select_turn<'a>(
    selected: &mut Vec<HistoryTurn<'a>>,
    chars: &mut usize,
    messages: &mut usize,
    turn: &HistoryTurn<'a>,
) -> bool {
    let turn_chars = turn.user.content.chars().count() + turn.assistant.content.chars().count();
    if *messages + 2 > HISTORY_MESSAGE_LIMIT || *chars + turn_chars > HISTORY_CHAR_LIMIT {
        return false;
    }
    *chars += turn_chars;
    *messages += 2;
    selected.push(*turn);
    true
}

fn try_select_anchor(
    selected: &mut Vec<AnchorMessage>,
    chars: &mut usize,
    anchor_chars: &mut usize,
    messages: &mut usize,
    turn: &HistoryTurn<'_>,
) -> bool {
    if *messages + 1 > HISTORY_MESSAGE_LIMIT
        || *chars >= HISTORY_CHAR_LIMIT
        || *anchor_chars >= HISTORY_ANCHOR_CHAR_LIMIT
    {
        return false;
    }
    let remaining_chars =
        (HISTORY_CHAR_LIMIT - *chars).min(HISTORY_ANCHOR_CHAR_LIMIT - *anchor_chars);
    let content = truncate_history_content(&turn.user.content, remaining_chars);
    let content_chars = content.chars().count();
    if content_chars == 0 || *chars + content_chars > HISTORY_CHAR_LIMIT {
        return false;
    }
    *chars += content_chars;
    *anchor_chars += content_chars;
    *messages += 1;
    selected.push(AnchorMessage {
        index: turn.index,
        role: turn.user.role.clone(),
        content,
        chars: content_chars,
    });
    true
}

fn remove_anchor(
    anchors: &mut Vec<AnchorMessage>,
    chars: &mut usize,
    anchor_chars: &mut usize,
    messages: &mut usize,
    index: usize,
) -> Option<AnchorMessage> {
    let position = anchors.iter().position(|anchor| anchor.index == index)?;
    let anchor = anchors.remove(position);
    *chars -= anchor.chars;
    *anchor_chars -= anchor.chars;
    *messages -= 1;
    Some(anchor)
}

fn restore_anchor(
    anchors: &mut Vec<AnchorMessage>,
    chars: &mut usize,
    anchor_chars: &mut usize,
    messages: &mut usize,
    anchor: AnchorMessage,
) {
    *chars += anchor.chars;
    *anchor_chars += anchor.chars;
    *messages += 1;
    anchors.push(anchor);
}

fn contains_turn<T: TurnIndex>(turns: &[T], index: usize) -> bool {
    turns.iter().any(|turn| turn.index() == index)
}

trait TurnIndex {
    fn index(&self) -> usize;
}

impl TurnIndex for HistoryTurn<'_> {
    fn index(&self) -> usize {
        self.index
    }
}

impl TurnIndex for AnchorMessage {
    fn index(&self) -> usize {
        self.index
    }
}

fn truncate_history_content(content: &str, max_chars: usize) -> String {
    let content_chars = content.chars().count();
    if content_chars <= max_chars {
        return content.to_string();
    }

    let marker_chars = HISTORY_TRUNCATION_MARKER.chars().count();
    if max_chars <= marker_chars {
        return content.chars().take(max_chars).collect();
    }

    let keep_chars = max_chars - marker_chars;
    let head_chars = keep_chars / 2;
    let tail_chars = keep_chars - head_chars;
    let head = content.chars().take(head_chars).collect::<String>();
    let tail = content
        .chars()
        .rev()
        .take(tail_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("{head}{HISTORY_TRUNCATION_MARKER}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_old_history_as_complete_turns() {
        let entries = (0..12)
            .flat_map(|i| {
                [
                    HistoryEntry::new("user", format!("u{i}")),
                    HistoryEntry::new("assistant", format!("a{i}")),
                ]
            })
            .collect::<Vec<_>>();
        let messages = history_messages(&entries);
        let roles = messages.iter().map(|m| m.role.as_str()).collect::<Vec<_>>();
        assert_eq!(roles.len(), 20);
        assert_eq!(messages[0].content, "u2");
        assert_eq!(messages[19].content, "a11");
    }

    #[test]
    fn keeps_stdin_anchor_when_recent_window_drops_old_turns() {
        let anchor = HistoryEntry::new("user", "inspect\n\nInput:\nCREATE TABLE payments (...);")
            .with_stdin_bytes(32);
        let mut entries = vec![anchor, HistoryEntry::new("assistant", "schema noted")];
        entries.extend((0..12).flat_map(|i| {
            [
                HistoryEntry::new("user", format!("follow up {i}")),
                HistoryEntry::new("assistant", format!("answer {i}")),
            ]
        }));

        let contents = history_messages(&entries)
            .into_iter()
            .map(|message| message.content)
            .collect::<Vec<_>>();

        assert!(contents.contains(&"inspect\n\nInput:\nCREATE TABLE payments (...);".into()));
        assert!(contents.contains(&"follow up 3".into()));
        assert!(!contents.contains(&"follow up 2".into()));
    }

    #[test]
    fn keeps_assistant_reply_when_stdin_turn_is_recent() {
        let entries = vec![
            HistoryEntry::new("user", "inspect\n\nInput:\nCREATE TABLE payments (...);")
                .with_stdin_bytes(32),
            HistoryEntry::new("assistant", "payments has a status column"),
        ];

        let contents = history_messages(&entries)
            .into_iter()
            .map(|message| message.content)
            .collect::<Vec<_>>();

        assert!(contents.contains(&"inspect\n\nInput:\nCREATE TABLE payments (...);".into()));
        assert!(contents.contains(&"payments has a status column".into()));
    }

    #[test]
    fn truncates_oversized_stdin_anchor_instead_of_dropping_it() {
        let anchor_content = format!(
            "inspect\n\nInput:\nCREATE TABLE payments (...);\n{}tail marker",
            "x".repeat(HISTORY_CHAR_LIMIT)
        );
        let entries = vec![
            HistoryEntry::new("user", anchor_content).with_stdin_bytes(HISTORY_CHAR_LIMIT + 48),
            HistoryEntry::new("assistant", "schema noted"),
        ];

        let anchor = history_messages(&entries)
            .into_iter()
            .find(|message| message.content.contains("[history truncated]"))
            .expect("oversized anchor should be retained as truncated context");

        assert!(anchor.content.contains("CREATE TABLE payments"));
        assert!(anchor.content.contains("tail marker"));
        assert!(anchor.content.chars().count() <= HISTORY_CHAR_LIMIT);
    }

    #[test]
    fn oversized_anchor_leaves_room_for_recent_turns() {
        let anchor_content = format!(
            "inspect\n\nInput:\nCREATE TABLE payments (...);\n{}tail marker",
            "x".repeat(HISTORY_CHAR_LIMIT)
        );
        let entries = vec![
            HistoryEntry::new("user", anchor_content).with_stdin_bytes(HISTORY_CHAR_LIMIT + 48),
            HistoryEntry::new("assistant", "schema noted"),
            HistoryEntry::new("user", "use status = success"),
            HistoryEntry::new("assistant", "filter on status"),
        ];

        let contents = history_messages(&entries)
            .into_iter()
            .map(|message| message.content)
            .collect::<Vec<_>>();

        assert!(
            contents
                .iter()
                .any(|content| content.contains("tail marker"))
        );
        assert!(contents.contains(&"use status = success".into()));
        assert!(contents.contains(&"filter on status".into()));
    }
}
