use axum::{
    extract::{Extension, Json, State},
    http::StatusCode,
    response::sse::{Event, Sse},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use futures_util::{stream, StreamExt};
use rig::{
    agent::MultiTurnStreamItem,
    completion::Message as RigMessage,
    streaming::{Item, StreamEvent},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::convert::Infallible;
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use tower_http::limit::RequestBodyLimitLayer;

use crate::agent::Registry;
use crate::auth::authorize;

#[derive(Clone, Debug)]
pub struct RequestLimits {
    pub max_request_bytes: usize,
    pub max_concurrent_requests: usize,
    pub timeout: Duration,
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self {
            max_request_bytes: 1_048_576,
            max_concurrent_requests: 64,
            timeout: Duration::from_secs(120),
        }
    }
}

#[derive(Clone)]
struct RequestControl {
    semaphore: Arc<Semaphore>,
    timeout: Duration,
}

pub fn router(agents: Registry, api_key: Option<String>, limits: RequestLimits) -> Router {
    let control = RequestControl {
        semaphore: Arc::new(Semaphore::new(limits.max_concurrent_requests)),
        timeout: limits.timeout,
    };
    let api = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .with_state(agents)
        .layer(Extension(control))
        .layer(RequestBodyLimitLayer::new(limits.max_request_bytes))
        .route_layer(axum::middleware::from_fn_with_state(api_key, authorize));

    Router::new().route("/healthz", get(health)).merge(api)
}

async fn health() -> &'static str {
    "ok"
}

/// Advertises configured agents in the OpenAI `/v1/models` shape, so existing
/// OpenAI SDKs can discover them without custom code.
async fn models(State(agents): State<Registry>) -> Json<Value> {
    let data: Vec<Value> = agents
        .keys()
        .map(|name| json!({"id": name, "object": "model", "owned_by": "riggate"}))
        .collect();
    Json(json!({"object": "list", "data": data}))
}

#[derive(Debug, Deserialize)]
struct ChatRequest {
    /// Which configured agent to run.
    model: String,
    messages: Vec<Message>,
    #[serde(default)]
    stream: bool,
}

#[derive(Debug, Deserialize)]
struct Message {
    role: String,
    #[serde(default)]
    content: String,
}

#[derive(Debug, Serialize)]
struct ChatChoice {
    index: u8,
    message: OutMessage,
    finish_reason: &'static str,
}

#[derive(Debug, Serialize)]
struct OutMessage {
    role: &'static str,
    content: String,
}

#[derive(Debug, Serialize)]
struct UsageOut {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    total_tokens: Option<u64>,
}

/// The OpenAI chat-completion envelope, so any OpenAI SDK can read it.
#[derive(Debug, Serialize)]
struct ChatResponse {
    id: String,
    object: &'static str,
    model: String,
    choices: Vec<ChatChoice>,
    usage: UsageOut,
}

async fn chat(
    State(agents): State<Registry>,
    Extension(control): Extension<RequestControl>,
    Json(req): Json<ChatRequest>,
) -> Result<axum::response::Response, (StatusCode, Json<Value>)> {
    let agent = agents.get(&req.model).ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            &format!("unknown model '{}'; call GET /v1/models", req.model),
        )
    })?;

    let (prompt, history) = conversation(req.messages)?;
    let permit = control.semaphore.clone().try_acquire_owned().map_err(|_| {
        err(
            StatusCode::SERVICE_UNAVAILABLE,
            "server is at its concurrent request limit; retry later",
        )
    })?;

    if req.stream {
        let id = completion_id();
        let model = req.model.clone();
        let run = agent.prompt(prompt).history(history).stream();
        let timeout = control.timeout;
        let events = tokio_stream::StreamExt::timeout(run, timeout)
            .scan(false, |timed_out, item| {
                let is_timeout = item.is_err();
                if *timed_out {
                    futures_util::future::ready(None)
                } else {
                    *timed_out = is_timeout;
                    futures_util::future::ready(Some(item))
                }
            })
            .filter_map(move |item| {
                let id = id.clone();
                let model = model.clone();
                async move {
                    let chunk = match item {
                        Err(_) => json!({
                            "error": {
                                "message": "upstream request timed out",
                                "type": "timeout_error"
                            }
                        }),
                        Ok(Ok(MultiTurnStreamItem::StreamAssistantItem(Item::Event(
                            StreamEvent::Text { text, .. },
                        )))) => json!({
                            "id": id,
                            "object": "chat.completion.chunk",
                            "model": model,
                            "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
                        }),
                        Ok(Ok(MultiTurnStreamItem::FinalResponse(_))) => json!({
                            "id": id,
                            "object": "chat.completion.chunk",
                            "model": model,
                            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
                        }),
                        Ok(Err(error)) => json!({
                            "error": {
                                "message": format!("upstream error: {error}"),
                                "type": "riggate_error"
                            }
                        }),
                        Ok(Ok(_)) => return None,
                    };
                    Some(Ok::<Event, Infallible>(Event::default().data(chunk.to_string())))
                }
            })
            .chain(stream::iter([Ok::<Event, Infallible>(
                Event::default().data("[DONE]"),
            )]))
            .boxed();
        let events = stream::unfold((events, permit), |(mut events, permit)| async move {
            let next = events.next().await?;
            Some((next, (events, permit)))
        });
        return Ok(Sse::new(events).into_response());
    }

    match tokio::time::timeout(control.timeout, agent.prompt(prompt).history(history)).await {
        Ok(Ok(res)) => Ok(Json(json!(ChatResponse {
            id: completion_id(),
            object: "chat.completion",
            model: req.model.clone(),
            choices: vec![ChatChoice {
                index: 0,
                message: OutMessage {
                    role: "assistant",
                    content: res.output().to_string(),
                },
                finish_reason: "stop",
            }],
            usage: UsageOut {
                prompt_tokens: res.usage.input_tokens,
                completion_tokens: res.usage.output_tokens,
                total_tokens: res.usage.total_tokens,
            },
        }))
        .into_response()),
        Ok(Err(e)) => Err(err(
            StatusCode::BAD_GATEWAY,
            &format!("upstream error: {e}"),
        )),
        Err(_) => Err(err(
            StatusCode::GATEWAY_TIMEOUT,
            "upstream request timed out",
        )),
    }
}

fn conversation(
    messages: Vec<Message>,
) -> Result<(String, Vec<RigMessage>), (StatusCode, Json<Value>)> {
    let prompt_index = messages
        .len()
        .checked_sub(1)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "no messages in request"))?;
    if messages[prompt_index].role != "user" {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "the last message must have the user role",
        ));
    }
    let prompt = messages[prompt_index].content.clone();
    let history = messages[..prompt_index]
        .iter()
        .map(|message| match message.role.as_str() {
            "system" => Ok(RigMessage::system(message.content.clone())),
            "user" => Ok(RigMessage::user(message.content.clone())),
            "assistant" => Ok(RigMessage::assistant(message.content.clone())),
            role => Err(err(
                StatusCode::BAD_REQUEST,
                &format!(
                    "unsupported message role '{role}'; supported roles are system, user, assistant"
                ),
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((prompt, history))
}

fn completion_id() -> String {
    format!(
        "chatcmpl-{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

fn err(code: StatusCode, msg: &str) -> (StatusCode, Json<Value>) {
    (
        code,
        Json(json!({"error": {"message": msg, "type": "riggate_error"}})),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    fn message(role: &str, content: &str) -> Message {
        Message {
            role: role.into(),
            content: content.into(),
        }
    }

    #[test]
    fn conversation_uses_last_user_message_as_prompt_and_preserves_history() {
        let (prompt, history) = conversation(vec![
            message("system", "Be concise"),
            message("user", "Earlier question"),
            message("assistant", "Earlier answer"),
            message("user", "Current question"),
        ])
        .expect("valid conversation");

        assert_eq!(prompt, "Current question");
        assert_eq!(history.len(), 3);
    }

    #[test]
    fn conversation_rejects_empty_messages() {
        let (status, _) = conversation(vec![]).unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn conversation_requires_last_message_to_be_user() {
        let (status, _) = conversation(vec![message("assistant", "not a prompt")]).unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn conversation_rejects_unsupported_history_role() {
        let (status, _) = conversation(vec![
            message("tool", "unsupported"),
            message("user", "question"),
        ])
        .unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn health_is_open_but_models_requires_the_bearer_token() {
        let registry = crate::agent::test_registry("http://127.0.0.1:1/v1");
        let app = router(
            registry,
            Some("test-token".into()),
            RequestLimits::default(),
        );

        let health = app
            .clone()
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);

        let unauthorized = app
            .clone()
            .oneshot(Request::get("/v1/models").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let models = app
            .oneshot(
                Request::get("/v1/models")
                    .header("authorization", "Bearer test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(models.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn chat_completion_works_against_a_local_openai_compatible_server() {
        let completion = post(|| async {
            Json(json!({
                "id": "mock-completion",
                "object": "chat.completion",
                "created": 1,
                "model": "mock-model",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "mock answer"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6}
            }))
        });
        let mock_provider = Router::new()
            .route("/v1/chat/completions", completion.clone())
            .route("/chat/completions", completion);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server =
            tokio::spawn(async move { axum::serve(listener, mock_provider).await.unwrap() });
        let registry = crate::agent::test_registry(&format!("http://{address}/v1"));
        let app = router(registry, None, RequestLimits::default());

        let response = app
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"model":"mock","messages":[{"role":"user","content":"hello"}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let completion: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            completion["choices"][0]["message"]["content"],
            "mock answer"
        );
        assert_eq!(completion["model"], "mock");

        server.abort();
    }

    #[tokio::test]
    async fn rejects_requests_larger_than_the_configured_body_limit() {
        let registry = crate::agent::test_registry("http://127.0.0.1:1/v1");
        let app = router(
            registry,
            None,
            RequestLimits {
                max_request_bytes: 8,
                ..RequestLimits::default()
            },
        );
        let response = app
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from("0123456789"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn returns_gateway_timeout_when_provider_exceeds_the_deadline() {
        let slow_response = post(|| async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            Json(json!({
                "id": "slow",
                "object": "chat.completion",
                "created": 1,
                "model": "mock-model",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "late"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            }))
        });
        let provider = Router::new()
            .route("/v1/chat/completions", slow_response.clone())
            .route("/chat/completions", slow_response);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
        let registry = crate::agent::test_registry(&format!("http://{address}/v1"));
        let app = router(
            registry,
            None,
            RequestLimits {
                timeout: Duration::from_millis(10),
                ..RequestLimits::default()
            },
        );
        let response = app
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"model":"mock","messages":[{"role":"user","content":"hello"}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        server.abort();
    }

    #[tokio::test]
    async fn rejects_agent_calls_above_the_concurrency_limit() {
        let request_started = Arc::new(tokio::sync::Notify::new());
        let started = request_started.clone();
        let slow_response = post(move || {
            let started = started.clone();
            async move {
                started.notify_one();
                tokio::time::sleep(Duration::from_millis(100)).await;
                Json(json!({
                    "id": "slow",
                    "object": "chat.completion",
                    "created": 1,
                    "model": "mock-model",
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": "answer"}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                }))
            }
        });
        let provider = Router::new()
            .route("/v1/chat/completions", slow_response.clone())
            .route("/chat/completions", slow_response);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
        let registry = crate::agent::test_registry(&format!("http://{address}/v1"));
        let app = router(
            registry,
            None,
            RequestLimits {
                max_concurrent_requests: 1,
                timeout: Duration::from_secs(2),
                ..RequestLimits::default()
            },
        );
        let first = tokio::spawn(app.clone().oneshot(chat_request()));
        request_started.notified().await;
        let second = app.oneshot(chat_request()).await.unwrap();
        assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(first.await.unwrap().unwrap().status(), StatusCode::OK);
        server.abort();
    }

    fn chat_request() -> Request<Body> {
        Request::post("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"mock","messages":[{"role":"user","content":"hello"}]}"#,
            ))
            .unwrap()
    }
}
