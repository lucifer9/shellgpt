use crate::ai::OpenAiClient;
use crate::config::AiConfig;
use crate::context::ContextBlock;
use crate::conversation::{AssistantResponse, Turn, UserInput, request_digest};
use crate::error::ERR_BODY_TOO_LARGE;
use crate::ids;
use crate::projection::{AskMode as ProjectionAskMode, BeginAsk, ProjectionError, ProjectionTree};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

const ASK_BODY_LIMIT: usize = 4 * 1024 * 1024;
const CONTROL_BODY_LIMIT: usize = 64 * 1024;
const PREPARE_BODY_LIMIT: usize = 512 * 1024;

#[derive(Clone)]
pub struct RelayState {
    token: String,
    client: OpenAiClient,
    tree: ProjectionTree,
    provider_permits: Arc<Semaphore>,
    bootstrap_port: u16,
    timeout_seconds: u64,
}

impl RelayState {
    pub fn new(token: String, config: AiConfig) -> anyhow::Result<Self> {
        Ok(Self {
            token,
            client: OpenAiClient::new(config.clone())?,
            tree: ProjectionTree::new(config.max_projected_sessions),
            provider_permits: Arc::new(Semaphore::new(config.max_concurrent_requests)),
            bootstrap_port: 0,
            timeout_seconds: config.timeout.as_secs(),
        })
    }

    pub fn with_bootstrap_port(mut self, port: u16) -> Self {
        self.bootstrap_port = port;
        self
    }

    pub async fn create_root(&self, session_id: String) -> anyhow::Result<()> {
        self.tree.create_root(session_id).await?;
        Ok(())
    }
}

pub async fn bind_loopback(port: Option<u16>) -> anyhow::Result<TcpListener> {
    Ok(TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port.unwrap_or(0)))).await?)
}

pub fn app(state: RelayState) -> Router {
    Router::new()
        .route(
            "/v1/ask",
            post(ask).layer(DefaultBodyLimit::max(ASK_BODY_LIMIT + 1)),
        )
        .route(
            "/v1/session/activate",
            post(activate).layer(DefaultBodyLimit::max(CONTROL_BODY_LIMIT + 1)),
        )
        .route(
            "/v1/session/unregister",
            post(unregister).layer(DefaultBodyLimit::max(CONTROL_BODY_LIMIT + 1)),
        )
        .route(
            "/v1/session/cancel",
            post(cancel).layer(DefaultBodyLimit::max(CONTROL_BODY_LIMIT + 1)),
        )
        .route(
            "/v1/tunnel/prepare",
            post(prepare_tunnel).layer(DefaultBodyLimit::max(PREPARE_BODY_LIMIT + 1)),
        )
        .route("/v1/bootstrap/sh", get(bootstrap_sh))
        .with_state(state)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum AskMode {
    New,
    Continue,
}

#[derive(Debug, Deserialize)]
struct AskRequest {
    request_id: String,
    session_id: String,
    mode: AskMode,
    input: UserInput,
    context: ContextBlock,
}

async fn ask(State(state): State<RelayState>, headers: HeaderMap, body: Bytes) -> Response {
    if !authorized(&headers, &state.token) {
        return plain(StatusCode::UNAUTHORIZED, "Unauthorized.");
    }
    if body.len() > ASK_BODY_LIMIT {
        return plain(StatusCode::PAYLOAD_TOO_LARGE, ERR_BODY_TOO_LARGE);
    }
    let request: AskRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            return plain(
                StatusCode::BAD_REQUEST,
                format!("Invalid JSON request: {err}"),
            );
        }
    };
    if !ids::is_hex_id(&request.request_id) || !ids::is_hex_id(&request.session_id) {
        return plain(StatusCode::BAD_REQUEST, "Invalid request_id or session_id.");
    }
    if request.input.stdin.len() > crate::input::STDIN_LIMIT || request.input.rendered().is_empty()
    {
        return plain(StatusCode::BAD_REQUEST, "Invalid user input.");
    }
    let mut input = request.input;
    input.timestamp = crate::conversation::timestamp();
    let context = request.context.truncated();
    let mode = match request.mode {
        AskMode::New => "new",
        AskMode::Continue => "continue",
    };
    let digest = match serde_json::to_vec(&context) {
        Ok(context_json) => request_digest(mode, &input, &context_json),
        Err(err) => return plain(StatusCode::BAD_REQUEST, err.to_string()),
    };

    let lease = match state
        .tree
        .begin_ask(
            &request.session_id,
            match request.mode {
                AskMode::New => ProjectionAskMode::New,
                AskMode::Continue => ProjectionAskMode::Continue,
            },
            &request.request_id,
            &digest,
        )
        .await
    {
        Ok(BeginAsk::Replay(answer)) => return plain(StatusCode::OK, answer),
        Ok(BeginAsk::Lease(lease)) => lease,
        Err(error) => return projection_error_response(error),
    };

    let permit = match state.provider_permits.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            lease.cancel().await;
            return plain(
                StatusCode::TOO_MANY_REQUESTS,
                "Origin Relay concurrent request limit reached.",
            );
        }
    };
    let result = state
        .client
        .ask(&context, lease.conversation(), &input)
        .await;
    drop(permit);
    match result {
        Ok(answer) => {
            let commit = lease
                .commit(Turn {
                    request_id: request.request_id,
                    request_digest: digest,
                    user: input,
                    assistant: AssistantResponse::new(answer.clone()),
                })
                .await;
            match commit {
                Ok(()) => plain(StatusCode::OK, answer),
                Err(error) => plain(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
            }
        }
        Err(err) => {
            lease.cancel().await;
            plain(StatusCode::BAD_GATEWAY, format!("{err:#}"))
        }
    }
}

fn projection_error_response(error: ProjectionError) -> Response {
    let status = match error {
        ProjectionError::NotFound => StatusCode::NOT_FOUND,
        ProjectionError::Conversation(_) => StatusCode::INTERNAL_SERVER_ERROR,
        ProjectionError::InvalidSessionId => StatusCode::BAD_REQUEST,
        _ => StatusCode::CONFLICT,
    };
    plain(status, error.to_string())
}

#[derive(Deserialize)]
struct SessionRequest {
    session_id: String,
}

async fn activate(State(state): State<RelayState>, headers: HeaderMap, body: Bytes) -> Response {
    let session_id = match control_session_id(&state, &headers, &body) {
        Ok(session_id) => session_id,
        Err(response) => return *response,
    };
    match state.tree.activate(&session_id).await {
        Ok(()) => plain(StatusCode::OK, "OK"),
        Err(error) => projection_error_response(error),
    }
}

async fn unregister(State(state): State<RelayState>, headers: HeaderMap, body: Bytes) -> Response {
    let session_id = match control_session_id(&state, &headers, &body) {
        Ok(session_id) => session_id,
        Err(response) => return *response,
    };
    state.tree.unregister(&session_id).await;
    plain(StatusCode::OK, "OK")
}

async fn cancel(State(state): State<RelayState>, headers: HeaderMap, body: Bytes) -> Response {
    let session_id = match control_session_id(&state, &headers, &body) {
        Ok(session_id) => session_id,
        Err(response) => return *response,
    };
    match state.tree.cancel_pending(&session_id).await {
        Ok(()) => plain(StatusCode::OK, "OK"),
        Err(error) => projection_error_response(error),
    }
}

fn control_session_id(
    state: &RelayState,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<String, Box<Response>> {
    if !authorized(headers, &state.token) {
        return Err(Box::new(plain(StatusCode::UNAUTHORIZED, "Unauthorized.")));
    }
    if body.len() > CONTROL_BODY_LIMIT {
        return Err(Box::new(plain(
            StatusCode::PAYLOAD_TOO_LARGE,
            ERR_BODY_TOO_LARGE,
        )));
    }
    let request: SessionRequest = serde_json::from_slice(body)
        .map_err(|error| Box::new(plain(StatusCode::BAD_REQUEST, error.to_string())))?;
    if !ids::is_hex_id(&request.session_id) {
        return Err(Box::new(plain(
            StatusCode::BAD_REQUEST,
            "Invalid session_id.",
        )));
    }
    Ok(request.session_id)
}

#[derive(Deserialize)]
struct PrepareRequest {
    parent_session_id: String,
    ssh_args: Vec<String>,
    effective_config: String,
}
#[derive(Serialize)]
struct PrepareResponse {
    session_id: String,
    remote_command: String,
}

async fn prepare_tunnel(
    State(state): State<RelayState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !authorized(&headers, &state.token) {
        return plain(StatusCode::UNAUTHORIZED, "Unauthorized.");
    }
    if body.len() > PREPARE_BODY_LIMIT {
        return plain(StatusCode::PAYLOAD_TOO_LARGE, ERR_BODY_TOO_LARGE);
    }
    let request: PrepareRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => return plain(StatusCode::BAD_REQUEST, err.to_string()),
    };
    if let Err(err) = crate::tunnel::ssh::validate_effective_policy(
        &request.ssh_args,
        &request.effective_config,
        Some(state.bootstrap_port),
    ) {
        return plain(StatusCode::BAD_REQUEST, err.to_string());
    }
    let child = match ids::id128() {
        Ok(id) => id,
        Err(err) => return plain(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    };
    if let Err(err) = state
        .tree
        .create_child(&request.parent_session_id, child.clone())
        .await
    {
        return plain(StatusCode::CONFLICT, err.to_string());
    }
    let command = match crate::tunnel::ssh::remote_bootstrap_command(
        state.bootstrap_port,
        &state.token,
        &child,
        state.timeout_seconds,
    ) {
        Ok(command) => command,
        Err(err) => {
            let _ = state.tree.cancel_pending(&child).await;
            return plain(StatusCode::INTERNAL_SERVER_ERROR, err.to_string());
        }
    };
    json(
        StatusCode::OK,
        &PrepareResponse {
            session_id: child,
            remote_command: command,
        },
    )
}

async fn bootstrap_sh(State(state): State<RelayState>, headers: HeaderMap) -> Response {
    if !authorized(&headers, &state.token) {
        return plain(StatusCode::UNAUTHORIZED, "Unauthorized.");
    }
    let mut response = plain(StatusCode::OK, crate::tunnel::bootstrap::stage1_script());
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

fn json(status: StatusCode, value: &impl Serialize) -> Response {
    match serde_json::to_vec(value) {
        Ok(body) => {
            let mut response = plain(status, body);
            response.headers_mut().insert(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/json"),
            );
            response
        }
        Err(err) => plain(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}
fn plain(status: StatusCode, body: impl Into<Body>) -> Response {
    let mut response = (status, body.into()).into_response();
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}
fn authorized(headers: &HeaderMap, token: &str) -> bool {
    let Some(value) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let mut parts = value.split_ascii_whitespace();
    let (Some(scheme), Some(candidate), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    scheme.eq_ignore_ascii_case("bearer")
        && ids::is_session_token(candidate)
        && constant_time_eq(candidate, token)
}
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= (a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0)) as usize;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::{mpsc, oneshot};
    use tower::ServiceExt;

    fn config() -> AiConfig {
        AiConfig {
            endpoint: "http://127.0.0.1/v1/chat/completions".into(),
            api_key: "key".into(),
            model: "m".into(),
            system_prompt: None,
            proxy: None,
            timeout: Duration::from_secs(1),
            debug: false,
            max_projected_sessions: 2,
            max_concurrent_requests: 1,
        }
    }

    #[test]
    fn bearer_auth_accepts_case_insensitive_scheme_and_exact_token() {
        let token = "a".repeat(64);
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            format!("bEaReR {token}").parse().unwrap(),
        );
        assert!(authorized(&headers, &token));
        assert!(!authorized(&headers, &"b".repeat(64)));
    }

    fn padded_json(base: &str, len: usize) -> Vec<u8> {
        let mut body = base.as_bytes().to_vec();
        body.resize(len, b' ');
        body
    }

    async fn post_body(router: Router, path: &str, token: &str, body: Vec<u8>) -> Response {
        router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("Authorization", format!("Bearer {token}"))
                    .header("Content-Type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    fn ask_body(session_id: &str, request_id: &str) -> String {
        format!(
            r#"{{"request_id":"{request_id}","session_id":"{session_id}","mode":"new","input":{{"instruction":"hello","stdin":"","timestamp":"t"}},"context":{{"cwd":"","uname_s":"","uname_m":"","shell":"","os_release":"","sw_vers":""}}}}"#
        )
    }

    async fn controlled_provider(
        requests: usize,
    ) -> (
        SocketAddr,
        mpsc::Receiver<oneshot::Sender<()>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = mpsc::channel(requests);
        let server = tokio::spawn(async move {
            let mut handlers = Vec::new();
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().await.unwrap();
                let accepted_tx = accepted_tx.clone();
                handlers.push(tokio::spawn(async move {
                    let mut request = [0_u8; 4096];
                    let _ = stream.read(&mut request).await;
                    let (release_tx, release_rx) = oneshot::channel();
                    accepted_tx.send(release_tx).await.unwrap();
                    let _ = release_rx.await;
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
                }));
            }
            for handler in handlers {
                let _ = handler.await;
            }
        });
        (address, accepted_rx, server)
    }

    async fn active_state(address: SocketAddr, sessions: &[&str]) -> (String, RelayState, Router) {
        let token = "a".repeat(64);
        let mut provider_config = config();
        provider_config.endpoint = format!("http://{address}/v1/chat/completions");
        provider_config.max_concurrent_requests = 2;
        provider_config.max_projected_sessions = sessions.len();
        let state = RelayState::new(token.clone(), provider_config).unwrap();
        for id in sessions {
            state.create_root((*id).into()).await.unwrap();
            state.tree.activate(id).await.unwrap();
        }
        let router = app(state.clone());
        (token, state, router)
    }

    #[tokio::test]
    async fn same_session_is_serial_while_different_sessions_run_concurrently() {
        let (address, mut accepted, same_server) = controlled_provider(1).await;
        let session = "0123456789abcdef";
        let (token, _state, router) = active_state(address, &[session]).await;
        let first_router = router.clone();
        let first_token = token.clone();
        let first = tokio::spawn(async move {
            post_body(
                first_router,
                "/v1/ask",
                &first_token,
                ask_body(session, "1111111111111111").into_bytes(),
            )
            .await
        });
        let release = accepted.recv().await.unwrap();
        let second = post_body(
            router,
            "/v1/ask",
            &token,
            ask_body(session, "2222222222222222").into_bytes(),
        )
        .await;
        assert_eq!(second.status(), StatusCode::CONFLICT);
        release.send(()).unwrap();
        assert_eq!(first.await.unwrap().status(), StatusCode::OK);
        same_server.await.unwrap();

        let (address, mut accepted, different_server) = controlled_provider(2).await;
        let first_session = "0123456789abcdef";
        let second_session = "1111111111111111";
        let (token, _state, router) = active_state(address, &[first_session, second_session]).await;
        let first_router = router.clone();
        let first_token = token.clone();
        let first = tokio::spawn(async move {
            post_body(
                first_router,
                "/v1/ask",
                &first_token,
                ask_body(first_session, "2222222222222222").into_bytes(),
            )
            .await
        });
        let second = tokio::spawn(async move {
            post_body(
                router,
                "/v1/ask",
                &token,
                ask_body(second_session, "3333333333333333").into_bytes(),
            )
            .await
        });
        let release_first = accepted.recv().await.unwrap();
        let release_second = accepted.recv().await.unwrap();
        release_first.send(()).unwrap();
        release_second.send(()).unwrap();
        assert_eq!(first.await.unwrap().status(), StatusCode::OK);
        assert_eq!(second.await.unwrap().status(), StatusCode::OK);
        different_server.await.unwrap();
    }

    #[tokio::test]
    async fn cancelling_ask_future_releases_its_session_lease() {
        let (address, mut accepted, server) = controlled_provider(1).await;
        let session = "0123456789abcdef";
        let (token, state, router) = active_state(address, &[session]).await;
        let request = tokio::spawn(async move {
            post_body(
                router,
                "/v1/ask",
                &token,
                ask_body(session, "1111111111111111").into_bytes(),
            )
            .await
        });
        let release = accepted.recv().await.unwrap();
        request.abort();
        let _ = request.await;
        for _ in 0..10 {
            if !state
                .tree
                .snapshot()
                .await
                .session(session)
                .unwrap()
                .has_lease
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            !state
                .tree
                .snapshot()
                .await
                .session(session)
                .unwrap()
                .has_lease
        );
        drop(release);
        server.abort();
    }

    #[tokio::test]
    async fn control_body_limit_is_route_local_and_does_not_mutate_session() {
        let token = "a".repeat(64);
        let state = RelayState::new(token.clone(), config()).unwrap();
        state.create_root("0123456789abcdef".into()).await.unwrap();
        let router = app(state.clone());
        let json = r#"{"session_id":"0123456789abcdef"}"#;

        let response = post_body(
            router.clone(),
            "/v1/session/activate",
            &token,
            padded_json(json, CONTROL_BODY_LIMIT),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);

        let response = post_body(
            router,
            "/v1/session/unregister",
            &token,
            padded_json(json, CONTROL_BODY_LIMIT + 1),
        )
        .await;
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert!(state.tree.snapshot().await.contains("0123456789abcdef"));
    }

    #[tokio::test]
    async fn ask_maps_projection_outcomes_to_existing_http_errors() {
        let token = "a".repeat(64);
        let state = RelayState::new(token.clone(), config()).unwrap();
        state.create_root("0123456789abcdef".into()).await.unwrap();
        let router = app(state.clone());

        let pending = post_body(
            router.clone(),
            "/v1/ask",
            &token,
            ask_body("0123456789abcdef", "1111111111111111").into_bytes(),
        )
        .await;
        assert_eq!(pending.status(), StatusCode::CONFLICT);
        assert_eq!(
            axum::body::to_bytes(pending.into_body(), usize::MAX)
                .await
                .unwrap(),
            "Projected Shell Session is not active."
        );

        let missing = post_body(
            router.clone(),
            "/v1/ask",
            &token,
            ask_body("2222222222222222", "3333333333333333").into_bytes(),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            axum::body::to_bytes(missing.into_body(), usize::MAX)
                .await
                .unwrap(),
            "Projected Shell Session not found."
        );

        state.tree.activate("0123456789abcdef").await.unwrap();
        let continue_body = ask_body("0123456789abcdef", "4444444444444444")
            .replace(r#""mode":"new""#, r#""mode":"continue""#);
        let no_previous = post_body(router, "/v1/ask", &token, continue_body.into_bytes()).await;
        assert_eq!(no_previous.status(), StatusCode::CONFLICT);
        assert_eq!(
            axum::body::to_bytes(no_previous.into_body(), usize::MAX)
                .await
                .unwrap(),
            crate::error::ERR_NO_PREVIOUS
        );
    }

    #[tokio::test]
    async fn prepare_command_failure_cancels_reserved_child_through_projection_module() {
        let token = "a".repeat(64);
        let mut invalid_timeout = config();
        invalid_timeout.timeout = Duration::from_secs(601);
        let state = RelayState::new(token.clone(), invalid_timeout).unwrap();
        state.create_root("0123456789abcdef".into()).await.unwrap();
        state.tree.activate("0123456789abcdef").await.unwrap();
        let router = app(state.clone());
        let body = br#"{"parent_session_id":"0123456789abcdef","ssh_args":["host"],"effective_config":""}"#;

        let response = post_body(router, "/v1/tunnel/prepare", &token, body.to_vec()).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let snapshot = state.tree.snapshot().await;
        assert_eq!(snapshot.len(), 1);
        assert!(
            snapshot
                .session("0123456789abcdef")
                .unwrap()
                .children
                .is_empty()
        );
    }

    #[tokio::test]
    async fn prepare_and_ask_have_independent_read_limits() {
        let token = "a".repeat(64);
        let state = RelayState::new(token.clone(), config()).unwrap();
        let router = app(state);

        let prepare_boundary = post_body(
            router.clone(),
            "/v1/tunnel/prepare",
            &token,
            padded_json("{}", PREPARE_BODY_LIMIT),
        )
        .await;
        assert_eq!(prepare_boundary.status(), StatusCode::BAD_REQUEST);
        let prepare = post_body(
            router.clone(),
            "/v1/tunnel/prepare",
            &token,
            padded_json("{}", PREPARE_BODY_LIMIT + 1),
        )
        .await;
        assert_eq!(prepare.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let ask_boundary = post_body(
            router.clone(),
            "/v1/ask",
            &token,
            padded_json("{}", ASK_BODY_LIMIT),
        )
        .await;
        assert_eq!(ask_boundary.status(), StatusCode::BAD_REQUEST);
        let ask = post_body(
            router.clone(),
            "/v1/ask",
            &token,
            padded_json("{}", ASK_BODY_LIMIT + 1),
        )
        .await;
        assert_eq!(ask.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let extractor_rejection = post_body(
            router,
            "/v1/ask",
            &token,
            padded_json("{}", ASK_BODY_LIMIT + 2),
        )
        .await;
        assert_eq!(extractor_rejection.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn relay_idempotency_commits_once() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let server_calls = calls.clone();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            server_calls.fetch_add(1, AtomicOrdering::SeqCst);
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).await;
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
        let mut provider_config = config();
        provider_config.endpoint = format!("http://{address}/v1/chat/completions");
        let token = "a".repeat(64);
        let state = RelayState::new(token.clone(), provider_config).unwrap();
        state.create_root("0123456789abcdef".into()).await.unwrap();
        state.tree.activate("0123456789abcdef").await.unwrap();
        let router = app(state.clone());
        let body = r#"{"request_id":"1111111111111111","session_id":"0123456789abcdef","mode":"new","input":{"instruction":"hello","stdin":"","timestamp":"t"},"context":{"cwd":"","uname_s":"","uname_m":"","shell":"","os_release":"","sw_vers":""}}"#;
        for _ in 0..2 {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/ask")
                        .header("Authorization", format!("Bearer {token}"))
                        .header("Content-Type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
    }
}
