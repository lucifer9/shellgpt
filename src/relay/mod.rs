use crate::ai::OpenAiClient;
use crate::config::AiConfig;
use crate::context::ContextBlock;
use crate::error::{ERR_BODY_TOO_LARGE, ERR_BUSY};
use crate::history::HistoryEntry;
use crate::ids;
use crate::input::UserInput;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

const BODY_LIMIT: usize = 1024 * 1024;

#[derive(Clone)]
pub struct RelayState {
    token: String,
    client: OpenAiClient,
    busy: Arc<Mutex<bool>>,
}

impl RelayState {
    pub fn new(token: String, config: AiConfig) -> anyhow::Result<Self> {
        Ok(Self {
            token,
            client: OpenAiClient::new(config)?,
            busy: Arc::new(Mutex::new(false)),
        })
    }
}

pub async fn bind_loopback(port: Option<u16>) -> anyhow::Result<TcpListener> {
    Ok(TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port.unwrap_or(0)))).await?)
}

pub fn app(state: RelayState) -> Router {
    Router::new()
        .route("/v1/ask", post(ask))
        .route("/v1/bootstrap/sh", get(bootstrap_sh))
        .with_state(state)
}

#[derive(Debug, Deserialize)]
struct AskRequest {
    request_id: Option<String>,
    session_id: String,
    conversation_id: String,
    prompt: String,
    history: Vec<HistoryEntry>,
    context: ContextBlock,
}

async fn ask(State(state): State<RelayState>, headers: HeaderMap, body: Bytes) -> Response {
    if !authorized(&headers, &state.token) {
        return plain(StatusCode::UNAUTHORIZED, "Unauthorized.");
    }
    if body.len() > BODY_LIMIT {
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
    if let Some(request_id) = &request.request_id
        && !ids::is_hex_id(request_id)
    {
        return plain(StatusCode::BAD_REQUEST, "Invalid request_id.");
    }
    if !ids::is_hex_id(&request.session_id) {
        return plain(StatusCode::BAD_REQUEST, "Invalid session_id.");
    }
    if !ids::is_hex_id(&request.conversation_id) {
        return plain(StatusCode::BAD_REQUEST, "Invalid conversation_id.");
    }

    let _guard = match InFlightGuard::acquire(state.busy.clone()).await {
        Some(guard) => guard,
        None => return plain(StatusCode::CONFLICT, ERR_BUSY),
    };
    let input = UserInput {
        prompt: request.prompt,
        stdin_bytes: 0,
    };
    match state
        .client
        .ask(&request.context, &request.history, &input)
        .await
    {
        Ok(answer) => plain(StatusCode::OK, answer),
        Err(err) => plain(StatusCode::BAD_GATEWAY, format!("{err:#}")),
    }
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

fn plain(status: StatusCode, body: impl Into<Body>) -> Response {
    let mut response = (status, body.into()).into_response();
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    let Some(value) = headers.get(axum::http::header::AUTHORIZATION) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
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
    let a = a.as_bytes();
    let b = b.as_bytes();
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let av = a.get(i).copied().unwrap_or(0);
        let bv = b.get(i).copied().unwrap_or(0);
        diff |= (av ^ bv) as usize;
    }
    diff == 0
}

struct InFlightGuard {
    busy: Arc<Mutex<bool>>,
}

impl InFlightGuard {
    async fn acquire(busy: Arc<Mutex<bool>>) -> Option<Self> {
        let mut locked = busy.lock().await;
        if *locked {
            return None;
        }
        *locked = true;
        drop(locked);
        Some(Self { busy })
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Ok(mut locked) = self.busy.try_lock() {
            *locked = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

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

    #[tokio::test]
    async fn bootstrap_requires_auth_and_has_no_cache() {
        let token = "a".repeat(64);
        let config = AiConfig {
            endpoint: "http://127.0.0.1/v1/chat/completions".into(),
            api_key: "key".into(),
            model: "m".into(),
            system_prompt: None,
            proxy: None,
            timeout: std::time::Duration::from_secs(1),
            debug: false,
        };
        let app = app(RelayState::new(token.clone(), config).unwrap());
        let unauthorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/bootstrap/sh")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let authorized = app
            .oneshot(
                Request::builder()
                    .uri("/v1/bootstrap/sh")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authorized.status(), StatusCode::OK);
        assert_eq!(authorized.headers()["cache-control"], "no-store");
    }
}
