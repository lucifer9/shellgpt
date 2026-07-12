use super::REQUEST_BODY_LIMIT;
use crate::config::AiConfig;
use crate::context::ContextBlock;
use crate::conversation::{Conversation, InputAnchor, Turn, UserInput};
use anyhow::ensure;
use serde::Serialize;
use std::collections::HashSet;

const RECENT_TURN_LIMIT: usize = 10;
const RECENT_TURN_BUDGET: usize = 200 * 1024;

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f64,
}

#[derive(Serialize)]
struct ChatMessage {
    role: String,
    content: String,
}

pub(super) fn build_body(
    config: &AiConfig,
    context: &ContextBlock,
    conversation: &Conversation,
    input: &UserInput,
) -> anyhow::Result<Vec<u8>> {
    let mandatory = serialize(&render(config, context, input, &[], &[]))?;
    ensure!(
        mandatory.len() <= REQUEST_BODY_LIMIT,
        "required AI request content exceeded 1 MiB limit."
    );

    let recent = recent_turns(conversation);
    let mut selected_turns = Vec::new();
    if let Some(newest) = recent.first() {
        let candidate = [*newest];
        if serialized_len(&render(config, context, input, &candidate, &[]))? <= REQUEST_BODY_LIMIT {
            selected_turns.push(*newest);
        }
    }

    let all_anchors = conversation.anchors().iter().collect::<Vec<_>>();
    let anchor_candidates = deduplicated_anchors(&all_anchors, &selected_turns);
    let anchor_count =
        fitting_anchor_prefix(config, context, input, &selected_turns, &anchor_candidates)?;
    let mut selected_anchors = anchor_candidates[..anchor_count].to_vec();

    if !selected_turns.is_empty() {
        for older in recent.iter().skip(1) {
            let mut candidate_turns = Vec::with_capacity(selected_turns.len() + 1);
            candidate_turns.push(*older);
            candidate_turns.extend_from_slice(&selected_turns);
            let candidate_anchors = deduplicated_anchors(&selected_anchors, &candidate_turns);
            if serialized_len(&render(
                config,
                context,
                input,
                &candidate_turns,
                &candidate_anchors,
            ))? > REQUEST_BODY_LIMIT
            {
                break;
            }
            selected_turns = candidate_turns;
            selected_anchors = candidate_anchors;
        }
    }

    let body = serialize(&render(
        config,
        context,
        input,
        &selected_turns,
        &selected_anchors,
    ))?;
    ensure!(
        body.len() <= REQUEST_BODY_LIMIT,
        "AI request exceeded 1 MiB limit."
    );
    Ok(body)
}

fn recent_turns(conversation: &Conversation) -> Vec<&Turn> {
    let mut selected = Vec::new();
    let mut bytes = 0;
    for turn in conversation.turns().iter().rev().take(RECENT_TURN_LIMIT) {
        if !selected.is_empty() && bytes + turn.logical_bytes() > RECENT_TURN_BUDGET {
            break;
        }
        bytes += turn.logical_bytes();
        selected.push(turn);
    }
    selected
}

fn fitting_anchor_prefix(
    config: &AiConfig,
    context: &ContextBlock,
    input: &UserInput,
    turns: &[&Turn],
    anchors: &[&InputAnchor],
) -> anyhow::Result<usize> {
    let mut low = 0;
    let mut high = anchors.len();
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        if serialized_len(&render(config, context, input, turns, &anchors[..middle]))?
            <= REQUEST_BODY_LIMIT
        {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    Ok(low)
}

fn deduplicated_anchors<'a>(anchors: &[&'a InputAnchor], turns: &[&Turn]) -> Vec<&'a InputAnchor> {
    let recent_inputs = turns
        .iter()
        .filter(|turn| !turn.user.stdin.is_empty())
        .map(|turn| (turn.user.instruction.as_str(), turn.user.stdin.as_str()))
        .collect::<HashSet<_>>();
    anchors
        .iter()
        .copied()
        .filter(|anchor| {
            !recent_inputs.contains(&(anchor.instruction.as_str(), anchor.stdin.as_str()))
        })
        .collect()
}

fn render(
    config: &AiConfig,
    context: &ContextBlock,
    input: &UserInput,
    turns: &[&Turn],
    anchors: &[&InputAnchor],
) -> ChatRequest {
    let mut messages = vec![ChatMessage {
        role: "system".into(),
        content: system_prompt(config.system_prompt.as_deref(), context),
    }];
    for turn in turns {
        messages.push(ChatMessage {
            role: "user".into(),
            content: turn.user.rendered(),
        });
        messages.push(ChatMessage {
            role: "assistant".into(),
            content: turn.assistant.content.clone(),
        });
    }
    let mut current = input.rendered();
    if !anchors.is_empty() {
        current.push_str("\n\nPrior stdin inputs retained as reference context:");
        for anchor in anchors {
            current.push_str("\n\n--- Input Anchor ---\n");
            current.push_str(&anchor.rendered());
        }
    }
    messages.push(ChatMessage {
        role: "user".into(),
        content: current,
    });
    ChatRequest {
        model: config.model.clone(),
        messages,
        temperature: 0.2,
    }
}

fn serialize(request: &ChatRequest) -> anyhow::Result<Vec<u8>> {
    Ok(serde_json::to_vec(request)?)
}

fn serialized_len(request: &ChatRequest) -> anyhow::Result<usize> {
    Ok(serialize(request)?.len())
}

fn system_prompt(extra: Option<&str>, context: &ContextBlock) -> String {
    let mut prompt = String::from(
        "You are sgpt, a concise shell assistant. Suggest shell commands; do not claim commands were executed. Respect detected OS, shell, and cwd context. Do not assume GNU tools on macOS. Prefer concise, low-noise answers. For simple command suggestions, avoid heavy Markdown unless multi-line formatting helps. Prefer read-only inspection commands or ask for clarification for destructive or production-impacting requests. Never present destructive commands as the default unless explicitly requested.",
    );
    if let Some(extra) = extra.filter(|s| !s.is_empty()) {
        prompt.push_str("\n\nAdditional user system prompt:\n");
        prompt.push_str(extra);
    }
    prompt.push_str("\n\n");
    prompt.push_str(&context.render());
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::{AssistantResponse, InputAnchor, Turn};
    use std::time::Duration;

    fn config() -> AiConfig {
        AiConfig {
            endpoint: "https://api.example.com/v1/chat/completions".into(),
            api_key: "sk-test".into(),
            model: "test-model".into(),
            system_prompt: Some("extra".into()),
            proxy: None,
            timeout: Duration::from_secs(60),
            debug: false,
            max_projected_sessions: 64,
            max_concurrent_requests: 4,
        }
    }

    fn turn(id: usize, instruction: &str, stdin: &str, answer: &str) -> Turn {
        Turn {
            request_id: format!("{id:016x}"),
            request_digest: format!("digest-{id}"),
            user: UserInput {
                instruction: instruction.into(),
                stdin: stdin.into(),
                timestamp: format!("user-{id}"),
            },
            assistant: AssistantResponse {
                content: answer.into(),
                timestamp: format!("assistant-{id}"),
            },
        }
    }

    fn anchor(id: usize, instruction: &str, stdin: &str) -> InputAnchor {
        InputAnchor {
            instruction: instruction.into(),
            stdin: stdin.into(),
            timestamp: format!("anchor-{id}"),
        }
    }

    fn messages(body: &[u8]) -> Vec<(String, String)> {
        let value: serde_json::Value = serde_json::from_slice(body).unwrap();
        value["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|message| {
                (
                    message["role"].as_str().unwrap().to_string(),
                    message["content"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    #[test]
    fn required_content_over_limit_fails() {
        let error = build_body(
            &config(),
            &ContextBlock::default(),
            &Conversation::default(),
            &UserInput::new("x".repeat(REQUEST_BODY_LIMIT), ""),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("required"));
    }

    #[test]
    fn latest_turn_then_anchors_then_only_fitting_older_suffix() {
        let turns = vec![
            turn(0, "oldest", "", "oldest-answer"),
            turn(1, "older", "", "older-answer"),
            turn(2, "latest", "", "latest-answer"),
        ];
        let anchors = vec![
            anchor(0, "anchor-a", "stdin-a"),
            anchor(1, "anchor-b", "stdin-b"),
        ];
        let conversation = Conversation::from_parts(turns, anchors).unwrap();
        let config = config();
        let context = ContextBlock::default();
        let empty = UserInput::new("", "");
        let recent = recent_turns(&conversation);
        let newest_two = [recent[1], recent[0]];
        let anchor_refs = conversation.anchors().iter().collect::<Vec<_>>();
        let exact_size = serialized_len(&render(
            &config,
            &context,
            &empty,
            &newest_two,
            &anchor_refs,
        ))
        .unwrap();
        let input = UserInput::new("x".repeat(REQUEST_BODY_LIMIT - exact_size), "");

        let body = build_body(&config, &context, &conversation, &input).unwrap();
        assert_eq!(body.len(), REQUEST_BODY_LIMIT);
        let messages = messages(&body);
        assert_eq!(messages[1].1, "older");
        assert_eq!(messages[2].1, "older-answer");
        assert_eq!(messages[3].1, "latest");
        assert_eq!(messages[4].1, "latest-answer");
        assert!(messages.last().unwrap().1.contains("anchor-a"));
        assert!(messages.last().unwrap().1.contains("anchor-b"));
        assert!(!messages.iter().any(|message| message.1 == "oldest"));
    }

    #[test]
    fn request_never_includes_half_a_turn() {
        let conversation = Conversation::from_parts(
            vec![turn(0, "large-user", "", &"a".repeat(300_000))],
            Vec::new(),
        )
        .unwrap();
        let config = config();
        let context = ContextBlock::default();
        let mandatory = build_body(
            &config,
            &context,
            &Conversation::default(),
            &UserInput::new("x".repeat(800_000), ""),
        )
        .unwrap();
        assert!(mandatory.len() < REQUEST_BODY_LIMIT);
        let body = build_body(
            &config,
            &context,
            &conversation,
            &UserInput::new("x".repeat(800_000), ""),
        )
        .unwrap();
        let messages = messages(&body);
        assert_eq!(messages.len(), 2);
        assert!(!messages.iter().any(|message| message.1 == "large-user"));
        assert!(!messages.iter().any(|message| message.1.len() == 300_000));
    }

    #[test]
    fn anchors_are_deduplicated_against_every_selected_recent_turn() {
        let conversation = Conversation::from_parts(
            vec![
                turn(0, "older", "same-older", "answer-0"),
                turn(1, "latest", "same-latest", "answer-1"),
            ],
            vec![
                anchor(0, "older", "same-older"),
                anchor(1, "latest", "same-latest"),
                anchor(2, "unique", "unique-stdin"),
            ],
        )
        .unwrap();
        let body = build_body(
            &config(),
            &ContextBlock::default(),
            &conversation,
            &UserInput::new("now", ""),
        )
        .unwrap();
        let messages = messages(&body);
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.1.contains("same-older"))
                .count(),
            1
        );
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.1.contains("same-latest"))
                .count(),
            1
        );
        assert!(messages.last().unwrap().1.contains("unique-stdin"));
    }

    #[test]
    fn serialized_json_bytes_account_for_escaping_and_utf8() {
        let config = config();
        let context = ContextBlock::default();
        let prefix = "quote=\" slash=\\ utf8=界 ";
        let base = build_body(
            &config,
            &context,
            &Conversation::default(),
            &UserInput::new(prefix, ""),
        )
        .unwrap();
        let input = UserInput::new(
            format!("{prefix}{}", "x".repeat(REQUEST_BODY_LIMIT - base.len())),
            "",
        );
        let body = build_body(&config, &context, &Conversation::default(), &input).unwrap();
        assert_eq!(body.len(), REQUEST_BODY_LIMIT);
        let text = String::from_utf8(body).unwrap();
        assert!(text.contains("quote=\\\" slash=\\\\ utf8=界"));
    }

    #[test]
    fn exact_limit_succeeds_and_one_byte_more_fails() {
        let config = config();
        let context = ContextBlock::default();
        let empty = build_body(
            &config,
            &context,
            &Conversation::default(),
            &UserInput::new("", ""),
        )
        .unwrap();
        let exact_len = REQUEST_BODY_LIMIT - empty.len();
        let exact = build_body(
            &config,
            &context,
            &Conversation::default(),
            &UserInput::new("x".repeat(exact_len), ""),
        )
        .unwrap();
        assert_eq!(exact.len(), REQUEST_BODY_LIMIT);
        let error = build_body(
            &config,
            &context,
            &Conversation::default(),
            &UserInput::new("x".repeat(exact_len + 1), ""),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("required"));
    }
}
