use axum::{
    extract::{Json, State},
    http::StatusCode,
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agent::Registry;

pub fn router(agents: Registry) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .with_state(agents)
}

async fn health() -> &'static str {
    "ok"
}

/// Advertises configured agents in the OpenAI `/v1/models` shape, so existing
/// OpenAI SDKs can discover them without custom code.
async fn models(State(agents): State<Registry>) -> Json<Value> {
    let data: Vec<Value> = agents
        .keys()
        .map(|name| json!({"id": name, "object": "model", "owned_by": "sallyport"}))
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
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if req.stream {
        return Err(err(
            StatusCode::NOT_IMPLEMENTED,
            "streaming is not implemented yet; send stream=false",
        ));
    }

    let agent = agents.get(&req.model).ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            &format!("unknown model '{}'; call GET /v1/models", req.model),
        )
    })?;

    // Everything except the last user turn is folded into the preamble by the
    // client; v0.1 runs a single-turn prompt.
    let prompt = req
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(|m| m.content.clone())
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "no user message in request"))?;

    match agent.prompt(&prompt).await {
        Ok(res) => Ok(Json(json!(ChatResponse {
            id: format!(
                "chatcmpl-{:x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ),
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
        }))),
        // A provider failure is not a client error — surface it as 502.
        Err(e) => Err(err(
            StatusCode::BAD_GATEWAY,
            &format!("upstream error: {e}"),
        )),
    }
}

fn err(code: StatusCode, msg: &str) -> (StatusCode, Json<Value>) {
    (
        code,
        Json(json!({"error": {"message": msg, "type": "sallyport_error"}})),
    )
}
