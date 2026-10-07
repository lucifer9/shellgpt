use crate::config::AiConfig;
use crate::conversation::{AssistantResponse, Turn, UserInput};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

pub fn ai_config(endpoint: impl Into<String>) -> AiConfig {
    AiConfig {
        endpoint: endpoint.into(),
        api_key: "sk-test".into(),
        model: "test-model".into(),
        system_prompt: None,
        proxy: None,
        timeout: Duration::from_secs(5),
        debug: false,
        max_projected_sessions: 64,
        max_concurrent_requests: 4,
    }
}

/// A complete Turn whose request id is `id` rendered as 16 hex digits.
pub fn turn(id: u64, stdin: &str, answer: &str) -> Turn {
    Turn {
        request_id: format!("{id:016x}"),
        request_digest: format!("digest-{id}"),
        user: UserInput {
            instruction: format!("instruction-{id}"),
            stdin: stdin.into(),
            timestamp: format!("user-{id}"),
        },
        assistant: AssistantResponse {
            content: answer.into(),
            timestamp: format!("assistant-{id}"),
        },
    }
}

pub fn chat_body(answer: &str) -> Vec<u8> {
    serde_json::json!({ "choices": [{ "message": { "content": answer } }] })
        .to_string()
        .into_bytes()
}

pub fn http_response(status: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

/// Serves exactly one provider request and returns its chat completions endpoint.
pub async fn provider(status: &'static str, body: Vec<u8>) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 8192];
        let _ = stream.read(&mut request).await.unwrap();
        stream
            .write_all(&http_response(status, &body))
            .await
            .unwrap();
    });
    (format!("http://{address}/v1/chat/completions"), server)
}
