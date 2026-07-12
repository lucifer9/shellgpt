use crate::ai::OpenAiClient;
use crate::context::ContextBlock;
use crate::conversation::{AssistantResponse, Turn, UserInput, request_digest};
use crate::error::ERR_NO_PREVIOUS;
use crate::ids;

mod history;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestMode {
    New,
    Continue,
}

impl RequestMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Continue => "continue",
        }
    }
}

#[derive(Debug)]
pub struct LocalRequest {
    pub mode: RequestMode,
    pub context: ContextBlock,
    pub input: UserInput,
}

pub struct LocalShellSession {
    client: OpenAiClient,
    debug: bool,
    #[cfg(test)]
    fixture: Option<(history::LocalSession, String)>,
}

impl LocalShellSession {
    pub fn new(client: OpenAiClient, debug: bool) -> Self {
        Self {
            client,
            debug,
            #[cfg(test)]
            fixture: None,
        }
    }

    pub async fn execute(&self, request: LocalRequest) -> anyhow::Result<String> {
        #[cfg(test)]
        if let Some((session, request_id)) = &self.fixture {
            return self
                .execute_resolved(session, request_id.clone(), request)
                .await;
        }
        let session = history::LocalSession::resolve().await?;
        let request_id = ids::id128()?;
        self.execute_resolved(&session, request_id, request).await
    }

    #[cfg(test)]
    fn at_for_tests(
        client: OpenAiClient,
        session: history::LocalSession,
        request_id: &str,
    ) -> Self {
        Self {
            client,
            debug: false,
            fixture: Some((session, request_id.into())),
        }
    }

    async fn execute_resolved(
        &self,
        session: &history::LocalSession,
        request_id: String,
        request: LocalRequest,
    ) -> anyhow::Result<String> {
        let _lock = history::SessionLock::acquire(session, &request_id, request.mode.as_str())?;
        let (conversation_id, conversation) = match request.mode {
            RequestMode::Continue => {
                let current = history::read_current(session)?
                    .ok_or_else(|| anyhow::anyhow!(ERR_NO_PREVIOUS))?;
                let conversation = history::load_conversation(session, &current)?;
                (current, conversation)
            }
            RequestMode::New => (ids::id128()?, Default::default()),
        };

        let answer = self
            .client
            .ask(&request.context, &conversation, &request.input)
            .await?;
        let digest = request_digest(
            request.mode.as_str(),
            &request.input,
            &serde_json::to_vec(&request.context)?,
        );
        let mut committed = conversation.clone();
        committed.commit(Turn {
            request_id,
            request_digest: digest,
            user: request.input,
            assistant: AssistantResponse::new(answer.clone()),
        })?;
        history::save_conversation(session, &conversation_id, &conversation, &committed)?;
        if request.mode == RequestMode::New {
            history::write_current(session, &conversation_id)?;
        }
        if self.debug {
            crate::debug::log("session_id", session.session_id());
            crate::debug::log("session_dir", session.dir().display());
        }
        Ok(answer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AiConfig;
    use crate::conversation::{AssistantResponse, Conversation};
    use std::fs;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::{mpsc, oneshot};

    const REQUEST_ID: &str = "1111111111111111";

    fn client(endpoint: String) -> OpenAiClient {
        OpenAiClient::new(AiConfig {
            endpoint,
            api_key: "key".into(),
            model: "model".into(),
            system_prompt: None,
            proxy: None,
            timeout: Duration::from_secs(5),
            debug: false,
            max_projected_sessions: 64,
            max_concurrent_requests: 4,
        })
        .unwrap()
    }

    fn fixture() -> (tempfile::TempDir, history::LocalSession) {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("conversations")).unwrap();
        let session = history::LocalSession::at_for_tests(temp.path().to_path_buf());
        (temp, session)
    }

    fn request(mode: RequestMode) -> LocalRequest {
        LocalRequest {
            mode,
            context: ContextBlock::default(),
            input: UserInput::new("hello", ""),
        }
    }

    fn turn(request_id: &str, digest: &str, answer: &str) -> Turn {
        Turn {
            request_id: request_id.into(),
            request_digest: digest.into(),
            user: UserInput::new("previous", ""),
            assistant: AssistantResponse::new(answer),
        }
    }

    async fn provider(
        status: &'static str,
        body: &'static [u8],
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 8192];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(body).await.unwrap();
        });
        (format!("http://{address}/v1/chat/completions"), server)
    }

    #[tokio::test]
    async fn provider_failure_writes_neither_conversation_nor_current_and_returns_no_answer() {
        let (_temp, session) = fixture();
        let (endpoint, server) = provider("500 Internal Server Error", b"provider failed").await;
        let runner = LocalShellSession::at_for_tests(client(endpoint), session.clone(), REQUEST_ID);

        assert!(runner.execute(request(RequestMode::New)).await.is_err());
        server.await.unwrap();
        assert!(history::read_current(&session).unwrap().is_none());
        assert_eq!(
            fs::read_dir(session.dir().join("conversations"))
                .unwrap()
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn commit_failure_keeps_conversation_and_current_unchanged() {
        let (_temp, session) = fixture();
        let conversation_id = "0123456789abcdef";
        let before = Conversation::default();
        let existing = Conversation::from_parts(
            vec![turn(REQUEST_ID, "different-digest", "old answer")],
            Vec::new(),
        )
        .unwrap();
        history::save_conversation(&session, conversation_id, &before, &existing).unwrap();
        history::write_current(&session, conversation_id).unwrap();
        let history_before = fs::read(session.conversation_path(conversation_id).unwrap()).unwrap();
        let current_before = fs::read(session.current_path()).unwrap();
        let (endpoint, server) = provider(
            "200 OK",
            br#"{"choices":[{"message":{"content":"new answer"}}]}"#,
        )
        .await;
        let runner = LocalShellSession::at_for_tests(client(endpoint), session.clone(), REQUEST_ID);

        assert!(
            runner
                .execute(request(RequestMode::Continue))
                .await
                .is_err()
        );
        server.await.unwrap();
        assert_eq!(
            fs::read(session.conversation_path(conversation_id).unwrap()).unwrap(),
            history_before
        );
        assert_eq!(fs::read(session.current_path()).unwrap(), current_before);
    }

    #[tokio::test]
    async fn new_success_sets_current_only_after_persisting_the_complete_turn() {
        let (_temp, session) = fixture();
        let (endpoint, server) = provider(
            "200 OK",
            br#"{"choices":[{"message":{"content":"answer"}}]}"#,
        )
        .await;
        let runner = LocalShellSession::at_for_tests(client(endpoint), session.clone(), REQUEST_ID);

        let answer = runner.execute(request(RequestMode::New)).await.unwrap();
        server.await.unwrap();
        let current = history::read_current(&session).unwrap().unwrap();
        let conversation = history::load_conversation(&session, &current).unwrap();
        assert_eq!(answer, "answer");
        assert_eq!(conversation.turns().len(), 1);
        assert_eq!(conversation.turns()[0].assistant.content, answer);
    }

    #[tokio::test]
    async fn continue_success_preserves_current_conversation_id() {
        let (_temp, session) = fixture();
        let conversation_id = "0123456789abcdef";
        let before = Conversation::default();
        let existing = Conversation::from_parts(
            vec![turn("2222222222222222", "digest", "old answer")],
            Vec::new(),
        )
        .unwrap();
        history::save_conversation(&session, conversation_id, &before, &existing).unwrap();
        history::write_current(&session, conversation_id).unwrap();
        let (endpoint, server) = provider(
            "200 OK",
            br#"{"choices":[{"message":{"content":"next answer"}}]}"#,
        )
        .await;
        let runner = LocalShellSession::at_for_tests(client(endpoint), session.clone(), REQUEST_ID);

        let answer = runner
            .execute(request(RequestMode::Continue))
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(answer, "next answer");
        assert_eq!(
            history::read_current(&session).unwrap().as_deref(),
            Some(conversation_id)
        );
        assert_eq!(
            history::load_conversation(&session, conversation_id)
                .unwrap()
                .turns()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn version_one_conversation_migrates_during_successful_continue() {
        let (_temp, session) = fixture();
        let conversation_id = "0123456789abcdef";
        let path = session.conversation_path(conversation_id).unwrap();
        fs::write(
            &path,
            "{\"role\":\"user\",\"content\":\"old\",\"ts\":\"u\"}\n{\"role\":\"assistant\",\"content\":\"answer\",\"ts\":\"a\"}\n",
        )
        .unwrap();
        history::write_current(&session, conversation_id).unwrap();
        let (endpoint, server) =
            provider("200 OK", br#"{"choices":[{"message":{"content":"next"}}]}"#).await;
        let runner = LocalShellSession::at_for_tests(client(endpoint), session.clone(), REQUEST_ID);

        runner
            .execute(request(RequestMode::Continue))
            .await
            .unwrap();
        server.await.unwrap();
        let persisted = fs::read_to_string(path).unwrap();
        assert!(persisted.lines().all(|line| line.contains("\"version\":2")));
        assert_eq!(
            history::load_conversation(&session, conversation_id)
                .unwrap()
                .turns()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn lock_covers_provider_call_and_failed_persistence() {
        let (_temp, session) = fixture();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://{}/v1/chat/completions",
            listener.local_addr().unwrap()
        );
        let (accepted_tx, mut accepted_rx) = mpsc::channel::<oneshot::Sender<()>>(1);
        let conversations = session.dir().join("conversations");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 8192];
            let _ = stream.read(&mut request).await.unwrap();
            let (release_tx, release_rx) = oneshot::channel();
            accepted_tx.send(release_tx).await.unwrap();
            release_rx.await.unwrap();
            fs::remove_dir(&conversations).unwrap();
            fs::write(&conversations, b"not a directory").unwrap();
            let body = br#"{"choices":[{"message":{"content":"answer"}}]}"#;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(body).await.unwrap();
        });
        let runner = LocalShellSession::at_for_tests(client(endpoint), session.clone(), REQUEST_ID);
        let transaction =
            tokio::spawn(async move { runner.execute(request(RequestMode::New)).await });

        let release = accepted_rx.recv().await.unwrap();
        assert!(session.dir().join("lock").is_dir());
        assert!(history::SessionLock::acquire(&session, "2222222222222222", "new").is_err());
        release.send(()).unwrap();
        assert!(transaction.await.unwrap().is_err());
        server.await.unwrap();
        assert!(!session.dir().join("lock").exists());
        assert!(history::read_current(&session).unwrap().is_none());
    }
}
