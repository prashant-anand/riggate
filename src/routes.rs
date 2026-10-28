use axum::{
    extract::{Json, State},
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

use crate::agent::Registry;
use crate::auth::authorize;

pub fn router(agents: Registry, api_key: Option<String>) -> Router {
    let api = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .with_state(agents)
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
    Json(req): Json<ChatRequest>,
) -> Result<axum::response::Response, (StatusCode, Json<Value>)> {
    let agent = agents.get(&req.model).ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            &format!("unknown model '{}'; call GET /v1/models", req.model),
        )
    })?;

    let (prompt, history) = conversation(req.messages)?;

    if req.stream {
        let id = completion_id();
        let model = req.model.clone();
        let run = agent.prompt(prompt).history(history).stream();
        let events = run
            .filter_map(move |item| {
                let id = id.clone();
                let model = model.clone();
                async move {
                    let chunk = match item {
                        Ok(MultiTurnStreamItem::StreamAssistantItem(Item::Event(
                            StreamEvent::Text { text, .. },
                        ))) => json!({
                            "id": id,
                            "object": "chat.completion.chunk",
                            "model": model,
                            "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
                        }),
                        Ok(MultiTurnStreamItem::FinalResponse(_)) => json!({
                            "id": id,
                            "object": "chat.completion.chunk",
                            "model": model,
                            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
                        }),
                        Err(error) => json!({
                            "error": {
                                "message": format!("upstream error: {error}"),
                                "type": "riggate_error"
                            }
                        }),
                        _ => return None,
                    };
                    Some(Ok::<Event, Infallible>(Event::default().data(chunk.to_string())))
                }
            })
            .chain(stream::iter([Ok::<Event, Infallible>(
                Event::default().data("[DONE]"),
            )]));
        return Ok(Sse::new(events).into_response());
    }

    match agent.prompt(prompt).history(history).await {
        Ok(res) => Ok(Json(json!(ChatResponse {
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
        // A provider failure is not a client error — surface it as 502.
        Err(e) => Err(err(
            StatusCode::BAD_GATEWAY,
            &format!("upstream error: {e}"),
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
}
