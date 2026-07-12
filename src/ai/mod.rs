use crate::config::AiConfig;
use crate::context::ContextBlock;
use crate::conversation::{Conversation, UserInput};
use crate::error::{ERR_AI_ANSWER_TOO_LARGE, ERR_AI_BODY_TOO_LARGE, ERR_AI_NO_TEXT};
use crate::redact::redact_provider_error;
use anyhow::{bail, ensure};

mod request;

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
        conversation: &Conversation,
        input: &UserInput,
    ) -> anyhow::Result<String> {
        let body = request::build_body(&self.config, context, conversation, input)?;
        let response = self
            .http
            .post(&self.config.endpoint)
            .bearer_auth(&self.config.api_key)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await?;
        let status = response.status();
        let bytes = read_bounded_response(response).await?;
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

async fn read_bounded_response(mut response: reqwest::Response) -> anyhow::Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > RESPONSE_BODY_LIMIT as u64)
    {
        bail!(ERR_AI_BODY_TOO_LARGE);
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len().saturating_add(chunk.len()) <= RESPONSE_BODY_LIMIT,
            ERR_AI_BODY_TOO_LARGE
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

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

    #[test]
    fn serializes_openai_compatible_request() {
        let body = request::build_body(
            &config(),
            &ContextBlock {
                cwd: "/tmp".into(),
                ..Default::default()
            },
            &Conversation::default(),
            &UserInput::new("hello", ""),
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["model"], "test-model");
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["messages"][1]["content"], "hello");
        assert!(body.len() <= REQUEST_BODY_LIMIT);
    }

    #[test]
    fn parse_response_accepts_only_text_message_content() {
        let ok = br#"{"choices":[{"message":{"content":"answer"}}]}"#;
        assert_eq!(parse_chat_response(ok).unwrap(), "answer");
        let bad = br#"{"choices":[{"message":{"tool_calls":[]}}]}"#;
        assert_eq!(
            parse_chat_response(bad).unwrap_err().to_string(),
            ERR_AI_NO_TEXT
        );
    }

    #[tokio::test]
    async fn provider_body_limit_is_streaming_for_chunked_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: application/json\r\n\r\n")
                .await
                .unwrap();
            let chunk = vec![b'x'; 64 * 1024];
            for _ in 0..40 {
                if stream.write_all(b"10000\r\n").await.is_err()
                    || stream.write_all(&chunk).await.is_err()
                    || stream.write_all(b"\r\n").await.is_err()
                {
                    break;
                }
            }
        });
        let mut config = config();
        config.endpoint = format!("http://{address}/v1/chat/completions");
        let client = OpenAiClient::new(config).unwrap();
        let err = client
            .ask(
                &ContextBlock::default(),
                &Conversation::default(),
                &UserInput::new("hello", ""),
            )
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(err, ERR_AI_BODY_TOO_LARGE);
    }
}
