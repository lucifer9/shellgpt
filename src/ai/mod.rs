use crate::config::AiConfig;
use crate::context::ContextBlock;
use crate::error::{ERR_AI_ANSWER_TOO_LARGE, ERR_AI_BODY_TOO_LARGE, ERR_AI_NO_TEXT};
use crate::history::HistoryEntry;
use crate::input::UserInput;
use crate::redact::redact_provider_error;
use anyhow::{bail, ensure};
use serde::{Deserialize, Serialize};

mod history_selection;

pub const REQUEST_BODY_LIMIT: usize = 1024 * 1024;
pub const RESPONSE_BODY_LIMIT: usize = 2 * 1024 * 1024;
pub const ASSISTANT_LIMIT: usize = 512 * 1024;

#[derive(Clone)]
pub struct OpenAiClient {
    config: AiConfig,
    http: reqwest::Client,
}

impl OpenAiClient {
    pub fn new(config: AiConfig) -> anyhow::Result<Self> {
        Ok(Self {
            http: config.reqwest_client()?,
            config,
        })
    }

    pub async fn ask(
        &self,
        context: &ContextBlock,
        history: &[HistoryEntry],
        input: &UserInput,
    ) -> anyhow::Result<String> {
        let request = build_chat_request(&self.config, context, history, &input.prompt)?;
        let mut body = serde_json::to_vec(&request)?;
        while body.len() > REQUEST_BODY_LIMIT {
            let Some(shorter) = request.with_less_history() else {
                bail!("AI request exceeded 1 MiB limit.");
            };
            body = serde_json::to_vec(&shorter)?;
        }

        let response = self
            .http
            .post(&self.config.endpoint)
            .bearer_auth(&self.config.api_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        ensure!(bytes.len() <= RESPONSE_BODY_LIMIT, ERR_AI_BODY_TOO_LARGE);
        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes);
            bail!(
                "AI provider returned HTTP {}: {}",
                status.as_u16(),
                redact_provider_error(&body, &[&self.config.api_key])
            );
        }
        parse_chat_response(&bytes)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    temperature: f64,
}

impl ChatRequest {
    fn with_less_history(&self) -> Option<Self> {
        if self.messages.len() <= 2 {
            return None;
        }
        let mut next = self.clone();
        next.messages.remove(1);
        Some(next)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ChatMessage {
    role: String,
    content: String,
}

pub fn build_chat_request(
    config: &AiConfig,
    context: &ContextBlock,
    history: &[HistoryEntry],
    current_prompt: &str,
) -> anyhow::Result<ChatRequest> {
    let mut messages = vec![ChatMessage {
        role: "system".into(),
        content: system_prompt(config.system_prompt.as_deref(), context),
    }];
    messages.extend(history_selection::history_messages(history));
    messages.push(ChatMessage {
        role: "user".into(),
        content: current_prompt.to_string(),
    });
    Ok(ChatRequest {
        model: config.model.clone(),
        messages,
        temperature: 0.2,
    })
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

pub fn parse_chat_response(bytes: &[u8]) -> anyhow::Result<String> {
    ensure!(bytes.len() <= RESPONSE_BODY_LIMIT, ERR_AI_BODY_TOO_LARGE);
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    let Some(content) = value
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_str())
    else {
        bail!(ERR_AI_NO_TEXT);
    };
    ensure!(content.len() <= ASSISTANT_LIMIT, ERR_AI_ANSWER_TOO_LARGE);
    Ok(content.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
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
        }
    }

    #[test]
    fn serializes_openai_compatible_request() {
        let request = build_chat_request(
            &config(),
            &ContextBlock {
                cwd: "/tmp".into(),
                ..Default::default()
            },
            &[],
            "hello",
        )
        .unwrap();
        let json = serde_json::to_value(request).unwrap();
        assert_eq!(json["model"], "test-model");
        assert_eq!(json["temperature"], 0.2);
        assert_eq!(json["messages"][0]["role"], "system");
        assert!(
            json["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("/tmp")
        );
        assert_eq!(json["messages"][1]["role"], "user");
        assert_eq!(json["messages"][1]["content"], "hello");
    }

    #[test]
    fn parse_response_accepts_only_text_message_content() {
        let ok = br#"{"choices":[{"message":{"content":"answer"}}]}"#;
        assert_eq!(parse_chat_response(ok).unwrap(), "answer");

        let bad = br#"{"choices":[{"message":{"tool_calls":[]}}]}"#;
        let err = parse_chat_response(bad).unwrap_err().to_string();
        assert_eq!(err, ERR_AI_NO_TEXT);
    }
}
