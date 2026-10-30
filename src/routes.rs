use axum::{
    extract::{rejection::JsonRejection, Extension, Json, State},
    http::{header, HeaderName, HeaderValue, StatusCode},
    response::sse::{Event, Sse},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use futures_util::{stream, StreamExt};
use rig::{
    agent::MultiTurnStreamItem,
    completion::{FinishReason, Message as RigMessage},
    streaming::{Item, StreamEvent},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU8, Ordering};
use std::{collections::HashMap, convert::Infallible};
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use tower_http::limit::RequestBodyLimitLayer;
use tracing::Instrument;

use crate::agent::Registry;
use crate::auth::authorize;

#[derive(Clone, Debug)]
pub struct RequestLimits {
    pub max_request_bytes: usize,
    pub max_concurrent_requests: usize,
    pub timeout: Duration,
    pub strict_requests: bool,
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self {
            max_request_bytes: 1_048_576,
            max_concurrent_requests: 64,
            timeout: Duration::from_secs(120),
            strict_requests: false,
        }
    }
}

#[derive(Clone)]
struct RequestControl {
    semaphore: Arc<Semaphore>,
    timeout: Duration,
    strict_requests: bool,
}

#[derive(Clone)]
struct AppState {
    agents: Registry,
    started_at: u64,
}

/// How a streaming response actually ended. Once the first token is sent the HTTP status is
/// already 200, so the status alone cannot say whether the stream finished, failed upstream,
/// timed out, or was abandoned by the client — which is the difference an operator cares about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StreamOutcome {
    Open,
    Completed,
    UpstreamError,
    TimedOut,
}

impl StreamOutcome {
    fn as_u8(self) -> u8 {
        match self {
            StreamOutcome::Open => 0,
            StreamOutcome::Completed => 1,
            StreamOutcome::UpstreamError => 2,
            StreamOutcome::TimedOut => 3,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => StreamOutcome::Completed,
            2 => StreamOutcome::UpstreamError,
            3 => StreamOutcome::TimedOut,
            _ => StreamOutcome::Open,
        }
    }

    /// `Open` at drop time means nothing terminal was ever recorded, so the client went away
    /// mid-stream. 499 is the conventional status for that.
    fn label(self) -> &'static str {
        match self {
            StreamOutcome::Open => "client_closed",
            StreamOutcome::Completed => "completed",
            StreamOutcome::UpstreamError => "upstream_error",
            StreamOutcome::TimedOut => "timed_out",
        }
    }
}

struct RequestCompletionGuard {
    span: tracing::Span,
    started: tokio::time::Instant,
    outcome: Arc<AtomicU8>,
}

impl RequestCompletionGuard {
    fn new(span: tracing::Span, started: tokio::time::Instant, outcome: Arc<AtomicU8>) -> Self {
        Self {
            span,
            started,
            outcome,
        }
    }
}

/// The terminal state of a streamed response, attached to the response so the value that drives
/// the log line can be asserted directly instead of being scraped back out of a log. Test-only:
/// nothing in the server reads it, and it must not become a way to smuggle state to callers.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct StreamOutcomeHandle(Arc<AtomicU8>);

#[cfg(test)]
impl StreamOutcomeHandle {
    pub(crate) fn outcome(&self) -> StreamOutcome {
        StreamOutcome::from_u8(self.0.load(Ordering::Relaxed))
    }
}

impl Drop for RequestCompletionGuard {
    fn drop(&mut self) {
        let outcome = StreamOutcome::from_u8(self.outcome.load(Ordering::Relaxed));
        self.span.record("status", StatusCode::OK.as_u16());
        self.span.record("outcome", outcome.label());
        self.span
            .record("duration_ms", self.started.elapsed().as_millis() as u64);
        if outcome == StreamOutcome::Completed {
            tracing::info!(parent: &self.span, "chat request completed");
        } else {
            tracing::warn!(parent: &self.span, "chat request ended without a final response");
        }
    }
}

pub fn router(agents: Registry, api_key: Option<String>, limits: RequestLimits) -> Router {
    let control = RequestControl {
        semaphore: Arc::new(Semaphore::new(limits.max_concurrent_requests)),
        timeout: limits.timeout,
        strict_requests: limits.strict_requests,
    };
    let api = Router::new()
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat))
        .with_state(AppState {
            agents,
            started_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs())
                .unwrap_or_default(),
        })
        .layer(Extension(control))
        .layer(RequestBodyLimitLayer::new(limits.max_request_bytes))
        .route_layer(axum::middleware::from_fn_with_state(api_key, authorize));

    Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(readiness))
        .merge(api)
}

async fn health() -> &'static str {
    "ok"
}

/// Readiness is only exposed after configuration, providers, and MCP connections have been
/// initialized. It does not make live requests to providers; use `/healthz` for liveness.
async fn readiness() -> Json<Value> {
    Json(json!({"status": "ready"}))
}

/// Advertises configured agents in the OpenAI `/v1/models` shape, so existing
/// OpenAI SDKs can discover them without custom code.
async fn models(State(state): State<AppState>) -> Json<Value> {
    let data: Vec<Value> = state
        .agents
        .keys()
        .map(|name| json!({"id": name, "object": "model", "created": state.started_at, "owned_by": "riggate"}))
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
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    max_completion_tokens: Option<u64>,
    top_p: Option<f64>,
    stop: Option<StopSequences>,
    frequency_penalty: Option<f64>,
    presence_penalty: Option<f64>,
    #[serde(flatten)]
    unsupported_fields: HashMap<String, Value>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum StopSequences {
    One(String),
    Many(Vec<String>),
}

impl StopSequences {
    fn as_value(&self) -> Value {
        match self {
            Self::One(value) => json!(value),
            Self::Many(values) => json!(values),
        }
    }
}

#[derive(Debug, Deserialize)]
struct Message {
    role: String,
    #[serde(default)]
    content: MessageContent,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(untagged)]
enum MessageContent {
    #[default]
    Empty,
    Text(String),
    Parts(Vec<TextPart>),
}

#[derive(Debug, Clone, Deserialize)]
struct TextPart {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

impl MessageContent {
    fn into_text(self) -> Result<String, (StatusCode, Json<Value>)> {
        match self {
            Self::Empty => Ok(String::new()),
            Self::Text(text) => Ok(text),
            Self::Parts(parts) => parts
                .into_iter()
                .map(|part| {
                    if part.kind == "text" {
                        part.text.ok_or_else(|| {
                            err(
                                StatusCode::BAD_REQUEST,
                                "text content parts must include a text field",
                            )
                        })
                    } else {
                        Err(err(
                            StatusCode::BAD_REQUEST,
                            "only text content parts are supported",
                        ))
                    }
                })
                .collect::<Result<Vec<_>, _>>()
                .map(|parts| parts.concat()),
        }
    }
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
    State(state): State<AppState>,
    Extension(control): Extension<RequestControl>,
    request: Result<Json<ChatRequest>, JsonRejection>,
) -> Result<axum::response::Response, (StatusCode, Json<Value>)> {
    let request_id = completion_id();
    let span = tracing::info_span!("chat.request", request_id = %request_id, agent = tracing::field::Empty, status = tracing::field::Empty, outcome = tracing::field::Empty, duration_ms = tracing::field::Empty, prompt_tokens = tracing::field::Empty, completion_tokens = tracing::field::Empty);
    let started = tokio::time::Instant::now();
    let outcome = Arc::new(AtomicU8::new(StreamOutcome::Open.as_u8()));
    let result = chat_inner(
        State(state),
        Extension(control),
        request,
        started,
        request_id.clone(),
        Arc::clone(&outcome),
    )
    .instrument(span.clone())
    .await;

    let mut response = match result {
        Ok(response) => response,
        Err((status, body)) => (status, body).into_response(),
    };
    // The id in the body, the id in the log line and the id on the wire are the same string, so
    // a client can quote one value and an operator can find the request it belongs to.
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response
            .headers_mut()
            .insert(HeaderName::from_static("x-request-id"), value);
    }
    #[cfg(test)]
    response
        .extensions_mut()
        .insert(StreamOutcomeHandle(Arc::clone(&outcome)));

    let status = response.status();
    let is_stream = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    if !is_stream {
        span.record("status", status.as_u16());
        span.record(
            "outcome",
            if status.is_success() {
                "completed"
            } else {
                "rejected"
            },
        );
        span.record("duration_ms", started.elapsed().as_millis() as u64);
        tracing::info!(parent: &span, "chat request completed");
    }
    Ok(response)
}

async fn chat_inner(
    State(state): State<AppState>,
    Extension(control): Extension<RequestControl>,
    request: Result<Json<ChatRequest>, JsonRejection>,
    started: tokio::time::Instant,
    request_id: String,
    outcome: Arc<AtomicU8>,
) -> Result<axum::response::Response, (StatusCode, Json<Value>)> {
    let Json(req) = request.map_err(|rejection| {
        let status = if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            StatusCode::PAYLOAD_TOO_LARGE
        } else {
            StatusCode::BAD_REQUEST
        };
        err(
            status,
            &format!("invalid request body: {}", rejection.body_text()),
        )
    })?;
    tracing::Span::current().record("agent", req.model.as_str());
    validate_chat_request(&req, control.strict_requests)?;
    let agent = state.agents.get(&req.model).ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            &format!("unknown model '{}'; call GET /v1/models", req.model),
        )
    })?;

    let (prompt, history) = conversation(req.messages)?;
    let permit = match control.semaphore.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return Ok((
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "1")],
                Json(json!({"error": {"message": "server is at its concurrent request limit; retry later", "type": "riggate_error"}})),
            )
                .into_response())
        }
    };
    let mut runner = agent.prompt(prompt).history(history);
    if let Some(temperature) = req.temperature {
        runner = runner.temperature(temperature);
    }
    if let Some(max_tokens) = req.max_tokens.or(req.max_completion_tokens) {
        runner = runner.max_tokens(max_tokens);
    }
    let mut additional_params = serde_json::Map::new();
    if let Some(top_p) = req.top_p {
        additional_params.insert("top_p".into(), json!(top_p));
    }
    if let Some(stop) = &req.stop {
        additional_params.insert("stop".into(), stop.as_value());
    }
    if let Some(penalty) = req.frequency_penalty {
        additional_params.insert("frequency_penalty".into(), json!(penalty));
    }
    if let Some(penalty) = req.presence_penalty {
        additional_params.insert("presence_penalty".into(), json!(penalty));
    }
    if !additional_params.is_empty() {
        runner = runner.merge_additional_params(additional_params);
    }

    if req.stream {
        let id = request_id;
        let model = req.model.clone();
        let max_tokens = req.max_tokens.or(req.max_completion_tokens);
        let run = runner.stream();
        let timeout = control.timeout;
        let span = tracing::Span::current();
        let completion_guard =
            RequestCompletionGuard::new(span.clone(), started, Arc::clone(&outcome));
        // Kept for the end-of-source check: the source ending with no terminal event is an
        // upstream truncation, not a client hanging up (a client hang-up drops this future
        // before the check can run).
        let end_outcome = Arc::clone(&outcome);
        // A per-item timeout alone lets a slow drip hold the permit forever, so
        // the same Instant bounds the whole stream as well.
        let deadline = tokio::time::Instant::now() + timeout;
        let events = tokio_stream::StreamExt::timeout(run, timeout)
            .map(move |item| match item {
                Ok(_) if tokio::time::Instant::now() >= deadline => Err(()),
                Ok(inner) => Ok(inner),
                Err(_) => Err(()),
            })
            .scan(false, |timed_out, item| {
                if *timed_out {
                    return futures_util::future::ready(None);
                }
                *timed_out = item.is_err();
                futures_util::future::ready(Some(item))
            })
            .filter_map(move |item| {
                let id = id.clone();
                let model = model.clone();
                let span = span.clone();
                let outcome = Arc::clone(&outcome);
                async move {
                    let chunk = match item {
                        Err(_) => {
                            outcome.store(StreamOutcome::TimedOut.as_u8(), Ordering::Relaxed);
                            json!({
                                "error": {
                                    "message": "upstream request timed out",
                                    "type": "timeout_error"
                                }
                            })
                        }
                        Ok(Ok(MultiTurnStreamItem::StreamAssistantItem(Item::Event(
                            StreamEvent::Text { text, .. },
                        )))) => json!({
                            "id": id,
                            "object": "chat.completion.chunk",
                            "model": model,
                            "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
                        }),
                        Ok(Ok(MultiTurnStreamItem::FinalResponse(res))) => {
                            outcome.store(StreamOutcome::Completed.as_u8(), Ordering::Relaxed);
                            span.record("prompt_tokens", res.usage.input_tokens);
                            span.record("completion_tokens", res.usage.output_tokens);
                            let finish_reason = completion_finish_reason(
                                res.completion_calls
                                    .last()
                                    .and_then(|call| call.finish_reason.clone()),
                                res.usage.output_tokens,
                                max_tokens,
                            );
                            json!({
                                "id": id,
                                "object": "chat.completion.chunk",
                                "model": model,
                                "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}]
                            })
                        }
                        Ok(Err(error)) => {
                            outcome.store(StreamOutcome::UpstreamError.as_u8(), Ordering::Relaxed);
                            json!({
                                "error": {
                                    "message": format!("upstream error: {error}"),
                                    "type": "riggate_error"
                                }
                            })
                        }
                        Ok(Ok(_)) => return None,
                    };
                    Some(Ok::<Event, Infallible>(Event::default().data(chunk.to_string())))
                }
            })
            .chain(
                stream::once(async move {
                    // Reached only when the source ended on its own. If nothing terminal was
                    // recorded, the upstream stopped mid-answer; telling the client [DONE]
                    // alone would report a truncated answer as complete.
                    if StreamOutcome::from_u8(end_outcome.load(Ordering::Relaxed))
                        == StreamOutcome::Open
                    {
                        end_outcome.store(StreamOutcome::UpstreamError.as_u8(), Ordering::Relaxed);
                        let chunk = json!({
                            "error": {
                                "message": "upstream stream ended without a final response",
                                "type": "riggate_error"
                            }
                        });
                        return Some(Ok::<Event, Infallible>(
                            Event::default().data(chunk.to_string()),
                        ));
                    }
                    None
                })
                .filter_map(|item| async move { item }),
            )
            .chain(stream::iter([Ok::<Event, Infallible>(
                Event::default().data("[DONE]"),
            )]))
            .boxed();
        let events = stream::unfold(
            (events, permit, completion_guard),
            |(mut events, permit, completion_guard)| async move {
                let next = events.next().await?;
                Some((next, (events, permit, completion_guard)))
            },
        );
        return Ok(Sse::new(events).into_response());
    }

    match tokio::time::timeout(control.timeout, runner).await {
        Ok(Ok(res)) => {
            tracing::Span::current().record("prompt_tokens", res.usage.input_tokens);
            tracing::Span::current().record("completion_tokens", res.usage.output_tokens);
            Ok(Json(json!(ChatResponse {
                id: request_id,
                object: "chat.completion",
                model: req.model.clone(),
                choices: vec![ChatChoice {
                    index: 0,
                    message: OutMessage {
                        role: "assistant",
                        content: res.output().to_string(),
                    },
                    finish_reason: completion_finish_reason(
                        res.completion_calls
                            .last()
                            .and_then(|call| call.finish_reason.clone()),
                        res.usage.output_tokens,
                        req.max_tokens.or(req.max_completion_tokens),
                    ),
                }],
                usage: UsageOut {
                    prompt_tokens: res.usage.input_tokens,
                    completion_tokens: res.usage.output_tokens,
                    total_tokens: res.usage.total_tokens,
                },
            }))
            .into_response())
        }
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

fn validate_chat_request(
    req: &ChatRequest,
    strict_requests: bool,
) -> Result<(), (StatusCode, Json<Value>)> {
    if strict_requests && !req.unsupported_fields.is_empty() {
        let mut names: Vec<_> = req.unsupported_fields.keys().cloned().collect();
        names.sort();
        return Err(err(
            StatusCode::BAD_REQUEST,
            &format!("unsupported request field(s): {}", names.join(", ")),
        ));
    }
    if req.max_tokens.is_some() && req.max_completion_tokens.is_some() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "specify only one of max_tokens or max_completion_tokens",
        ));
    }
    if req.max_tokens.or(req.max_completion_tokens) == Some(0) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "max_tokens must be greater than zero",
        ));
    }
    if req
        .temperature
        .is_some_and(|value| !(0.0..=2.0).contains(&value))
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "temperature must be between 0 and 2",
        ));
    }
    if req.top_p.is_some_and(|value| !(0.0..=1.0).contains(&value)) {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "top_p must be between 0 and 1",
        ));
    }
    if req
        .frequency_penalty
        .into_iter()
        .chain(req.presence_penalty)
        .any(|value| !(-2.0..=2.0).contains(&value))
    {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "frequency_penalty and presence_penalty must be between -2 and 2",
        ));
    }
    Ok(())
}

/// The finish reason an OpenAI client understands. A provider-reported reason wins outright:
/// reconstructing it from token counts mislabels providers that include reasoning tokens in
/// `output_tokens`, or answers that land on the cap by coincidence. Token counts are only used
/// when the provider said nothing at all, and only when they land exactly on a cap we set.
fn completion_finish_reason(
    reported: Option<FinishReason>,
    output_tokens: Option<u64>,
    max_tokens: Option<u64>,
) -> &'static str {
    match reported {
        Some(FinishReason::Length) => "length",
        Some(FinishReason::ToolCalls) => "tool_calls",
        Some(FinishReason::ContentFilter) => "content_filter",
        Some(FinishReason::Stop) => "stop",
        Some(FinishReason::Other(reason)) => {
            // Outside the normalised vocabulary. Answer with a plain stop so clients that match on
            // known values keep working, and keep the provider's spelling for the log.
            tracing::debug!(provider_finish_reason = %reason, "unmapped provider finish reason");
            "stop"
        }
        _ => match (max_tokens, output_tokens) {
            (Some(limit), Some(used)) if used == limit => {
                tracing::debug!(
                    used,
                    limit,
                    "provider reported no finish reason; reporting length from an exactly reached cap"
                );
                "length"
            }
            _ => "stop",
        },
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
    let prompt = messages[prompt_index].content.clone().into_text()?;
    let history = messages[..prompt_index]
        .iter()
        .map(|message| match message.role.as_str() {
            "system" => Ok(RigMessage::system(message.content.clone().into_text()?)),
            "user" => Ok(RigMessage::user(message.content.clone().into_text()?)),
            "assistant" => Ok(RigMessage::assistant(message.content.clone().into_text()?)),
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

    #[test]
    fn a_reported_finish_reason_wins_over_the_token_count() {
        // The provider said it stopped naturally, and the answer happens to land on the cap.
        assert_eq!(
            completion_finish_reason(Some(FinishReason::Stop), Some(64), Some(64)),
            "stop"
        );
        assert_eq!(
            completion_finish_reason(Some(FinishReason::Length), Some(64), Some(64)),
            "length"
        );
        assert_eq!(
            completion_finish_reason(Some(FinishReason::ToolCalls), None, None),
            "tool_calls"
        );
        assert_eq!(
            completion_finish_reason(Some(FinishReason::ContentFilter), None, None),
            "content_filter"
        );
        assert_eq!(
            completion_finish_reason(Some(FinishReason::Other("eos".into())), None, None),
            "stop"
        );
    }

    #[test]
    fn the_cap_is_only_evidence_when_nothing_was_reported() {
        assert_eq!(completion_finish_reason(None, Some(64), Some(64)), "length");
        assert_eq!(completion_finish_reason(None, Some(63), Some(64)), "stop");
        // Over the cap is not a truncation signal we invented; it is a provider counting
        // differently, and guessing "length" there would be wrong.
        assert_eq!(completion_finish_reason(None, Some(70), Some(64)), "stop");
        assert_eq!(completion_finish_reason(None, Some(10), None), "stop");
        assert_eq!(completion_finish_reason(None, None, None), "stop");
    }

    fn message(role: &str, content: &str) -> Message {
        Message {
            role: role.into(),
            content: MessageContent::Text(content.into()),
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

    #[test]
    fn chat_request_rejects_unsupported_fields_instead_of_ignoring_them() {
        let request: ChatRequest = serde_json::from_value(json!({
            "model": "mock",
            "messages": [{"role": "user", "content": "hello"}],
            "n": 2
        }))
        .expect("request decodes so the field can be reported");
        let (status, _) = validate_chat_request(&request, true).unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn chat_request_ignores_unknown_fields_by_default_but_can_be_strict() {
        let request: ChatRequest = serde_json::from_value(json!({
            "model": "mock",
            "messages": [{"role": "user", "content": "hello"}],
            "stream_options": {"include_usage": true}
        }))
        .unwrap();
        assert!(validate_chat_request(&request, false).is_ok());
        assert_eq!(
            validate_chat_request(&request, true).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn conversation_accepts_text_content_parts() {
        let request: ChatRequest = serde_json::from_value(json!({
            "model": "mock",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hello "}, {"type": "text", "text": "world"}]}
            ]
        }))
        .unwrap();
        let (prompt, history) = conversation(request.messages).unwrap();
        assert_eq!(prompt, "hello world");
        assert!(history.is_empty());
    }

    #[test]
    fn conversation_rejects_non_text_content_parts_with_a_clear_error() {
        let request: ChatRequest = serde_json::from_value(json!({
            "model": "mock",
            "messages": [{"role": "user", "content": [{"type": "image_url", "image_url": "x"}]}]
        }))
        .unwrap();
        let error = conversation(request.messages).unwrap_err();
        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert!(error.1 .0["error"]["message"]
            .as_str()
            .unwrap()
            .contains("only text content parts"));
    }

    #[test]
    fn chat_request_validates_generation_option_ranges_and_conflicts() {
        let conflicting: ChatRequest = serde_json::from_value(json!({
            "model": "mock",
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 10,
            "max_completion_tokens": 10
        }))
        .unwrap();
        assert_eq!(
            validate_chat_request(&conflicting, false).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );

        let invalid_temperature: ChatRequest = serde_json::from_value(json!({
            "model": "mock",
            "messages": [{"role": "user", "content": "hello"}],
            "temperature": 2.1
        }))
        .unwrap();
        assert_eq!(
            validate_chat_request(&invalid_temperature, false)
                .unwrap_err()
                .0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn finish_reason_reports_length_when_provider_or_token_limit_indicates_truncation() {
        assert_eq!(
            completion_finish_reason(Some(FinishReason::Length), Some(5), None),
            "length"
        );
        assert_eq!(completion_finish_reason(None, Some(5), Some(5)), "length");
        assert_eq!(
            completion_finish_reason(Some(FinishReason::Stop), Some(4), Some(5)),
            "stop"
        );
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

        let readiness = app
            .clone()
            .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(readiness.status(), StatusCode::OK);
        let readiness_body = axum::body::to_bytes(readiness.into_body(), usize::MAX)
            .await
            .unwrap();
        let readiness_json: Value = serde_json::from_slice(&readiness_body).unwrap();
        assert_eq!(readiness_json["status"], "ready");

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
    async fn malformed_json_returns_an_openai_shaped_bad_request() {
        let registry = crate::agent::test_registry("http://127.0.0.1:1/v1");
        let app = router(registry, None, RequestLimits::default());
        let response = app
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from("{"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"]["type"], "riggate_error");
        assert!(error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("invalid request body"));
    }

    #[tokio::test]
    async fn chat_completion_works_against_a_local_openai_compatible_server() {
        let received_request = Arc::new(std::sync::Mutex::new(None));
        let received = received_request.clone();
        let completion = post(move |Json(request): Json<Value>| {
            let received = received.clone();
            async move {
                *received.lock().unwrap() = Some(request);
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
                "usage": {"prompt_tokens": 4, "completion_tokens": 42, "total_tokens": 46}
                }))
            }
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
                        r#"{"model":"mock","messages":[{"role":"user","content":"hello"}],"temperature":0.3,"max_tokens":42,"top_p":0.8,"stop":["END"],"frequency_penalty":0.2}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let request_id = response
            .headers()
            .get("x-request-id")
            .expect("every response carries an x-request-id")
            .to_str()
            .unwrap()
            .to_string();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let completion: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            completion["choices"][0]["message"]["content"],
            "mock answer"
        );
        assert_eq!(
            completion["choices"][0]["finish_reason"], "stop",
            "the upstream reported a natural stop; a token count that happens to equal \
             max_tokens must not relabel it as truncation"
        );
        assert_eq!(completion["model"], "mock");
        assert_eq!(
            completion["id"], request_id,
            "the body id and the header must be the same value an operator can grep"
        );
        let upstream = received_request.lock().unwrap().clone().unwrap();
        assert_eq!(upstream["temperature"], 0.3);
        assert_eq!(upstream["max_tokens"], 42);
        assert_eq!(upstream["top_p"], 0.8);
        assert_eq!(upstream["stop"], json!(["END"]));
        assert_eq!(upstream["frequency_penalty"], 0.2);

        server.abort();
    }

    /// The end-to-end path: a model drives a permitted tool call and the round trip completes.
    ///
    /// What this proves: the gateway advertises the permitted tool to the model, executes it
    /// when the model asks, feeds the output back, and returns the model's final answer.
    ///
    /// What it does not prove: that a model will reliably *choose* a tool you want it to
    /// choose. Tool-selection quality varies by model and run, which is why the allowlist
    /// boundary is a separate, model-free test in `agent::tests` rather than an assertion on
    /// what the model happened to request here.
    #[tokio::test]
    async fn a_permitted_tool_is_invoked_by_the_model_and_its_result_returns_to_the_model() {
        use std::sync::atomic::AtomicUsize;

        let requests: Arc<std::sync::Mutex<Vec<Value>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let tool_invocations = Arc::new(AtomicUsize::new(0));

        let recorded = requests.clone();
        let completion = post(move |Json(request): Json<Value>| {
            let recorded = recorded.clone();
            async move {
                let turn = {
                    let mut requests = recorded.lock().unwrap();
                    requests.push(request);
                    requests.len()
                };
                if turn == 1 {
                    // Turn one: the model asks for the one tool it was offered.
                    Json(json!({
                        "id": "mock-tool-call",
                        "object": "chat.completion",
                        "created": 1,
                        "model": "mock-model",
                        "choices": [{
                            "index": 0,
                            "message": {
                                "role": "assistant",
                                "content": null,
                                "tool_calls": [{
                                    "id": "call_1",
                                    "type": "function",
                                    "function": {
                                        "name": "get_time",
                                        "arguments": "{\"city\":\"Hyderabad\"}"
                                    }
                                }]
                            },
                            "finish_reason": "tool_calls"
                        }],
                        "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
                    }))
                } else {
                    Json(json!({
                        "id": "mock-final",
                        "object": "chat.completion",
                        "created": 2,
                        "model": "mock-model",
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "The time is 12:00."},
                            "finish_reason": "stop"
                        }],
                        "usage": {"prompt_tokens": 20, "completion_tokens": 8, "total_tokens": 28}
                    }))
                }
            }
        });
        let provider = Router::new()
            .route("/v1/chat/completions", completion.clone())
            .route("/chat/completions", completion);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });

        let registry = crate::agent::test_registry_with_tools(
            &format!("http://{address}/v1"),
            Some(vec!["get_time".to_string()]),
            tool_invocations.clone(),
        );
        let app = router(registry, None, RequestLimits::default());
        let response = app
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"model":"mock","messages":[{"role":"user","content":"What time is it in Hyderabad?"}]}"#,
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
            completion["choices"][0]["message"]["content"], "The time is 12:00.",
            "the caller must receive the model's final answer, not the raw tool call"
        );

        let requests = requests.lock().unwrap().clone();
        assert_eq!(
            requests.len(),
            2,
            "a tool round trip is two provider calls: the model asks, then answers"
        );
        let offered = requests[0]["tools"].to_string();
        assert!(
            offered.contains("get_time"),
            "the permitted tool must be advertised to the model or it could never be chosen; \
             offered: {offered}"
        );
        assert_eq!(
            tool_invocations.load(Ordering::SeqCst),
            1,
            "the model asked once, so the gateway must execute the tool exactly once"
        );
        assert!(
            requests[1].to_string().contains("12:00"),
            "the tool's output must be fed back to the model, not swallowed; second request: {}",
            requests[1]
        );

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
    async fn streamed_completion_has_an_absolute_deadline_for_slow_drip_chunks() {
        let provider = Router::new()
            .route("/v1/chat/completions", post(slow_stream_response))
            .route("/chat/completions", post(slow_stream_response));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
        let registry = crate::agent::test_registry(&format!("http://{address}/v1"));
        let app = router(
            registry,
            None,
            RequestLimits {
                timeout: Duration::from_secs(1),
                ..RequestLimits::default()
            },
        );
        let started = tokio::time::Instant::now();
        let response = app
            .oneshot(
                Request::post("/v1/chat/completions")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"model":"mock","stream":true,"messages":[{"role":"user","content":"hello"}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = tokio::time::timeout(
            Duration::from_secs(2),
            axum::body::to_bytes(response.into_body(), usize::MAX),
        )
        .await
        .unwrap()
        .unwrap();
        let elapsed = started.elapsed();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(
            elapsed >= Duration::from_millis(900),
            "elapsed: {elapsed:?}"
        );
        assert!(elapsed < Duration::from_secs(2), "elapsed: {elapsed:?}");
        assert!(body.contains("upstream request timed out"), "{body}");
        assert!(body.contains("[DONE]"), "{body}");
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
        assert_eq!(second.headers()[header::RETRY_AFTER], "1");
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

    // --- stream outcomes: asserted through the value the log line is built from ---

    fn outcome_handle(response: &axum::response::Response) -> StreamOutcomeHandle {
        response
            .extensions()
            .get::<StreamOutcomeHandle>()
            .expect("every response carries its outcome handle")
            .clone()
    }

    async fn stream_router(timeout: Duration) -> (Router, tokio::task::JoinHandle<()>) {
        let provider = Router::new()
            .route("/v1/chat/completions", post(slow_stream_response))
            .route("/chat/completions", post(slow_stream_response));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
        let registry = crate::agent::test_registry(&format!("http://{address}/v1"));
        (
            router(
                registry,
                None,
                RequestLimits {
                    timeout,
                    ..RequestLimits::default()
                },
            ),
            server,
        )
    }

    fn stream_request() -> Request<Body> {
        Request::post("/v1/chat/completions")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"model":"mock","stream":true,"messages":[{"role":"user","content":"hello"}]}"#,
            ))
            .unwrap()
    }

    #[tokio::test]
    async fn an_abandoned_stream_ends_as_client_closed() {
        // The status is already 200 by the first token, so a client that hangs up mid-answer
        // would otherwise be indistinguishable from a request that finished.
        let (app, server) = stream_router(Duration::from_secs(30)).await;
        let response = app.oneshot(stream_request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let request_id = response
            .headers()
            .get("x-request-id")
            .expect("streams carry the request id too")
            .to_str()
            .unwrap()
            .to_string();
        let outcome = outcome_handle(&response);

        let mut stream = response.into_body().into_data_stream();
        let first = stream.next().await.expect("a first chunk").unwrap();
        assert!(
            String::from_utf8_lossy(&first).contains(&format!("\"id\":\"{request_id}\"")),
            "the chunk id must be the same value as the header"
        );
        assert_eq!(
            outcome.outcome(),
            StreamOutcome::Open,
            "nothing terminal has happened yet"
        );
        drop(stream);
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        assert_eq!(
            outcome.outcome(),
            StreamOutcome::Open,
            "an abandoned stream is recorded as still open, which the log renders as client_closed"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_stream_that_hits_the_deadline_ends_as_timed_out() {
        // A provider that never finishes: the request deadline is what ends this stream.
        let (app, server) = stream_router(Duration::from_secs(1)).await;
        let response = app.oneshot(stream_request()).await.unwrap();
        let outcome = outcome_handle(&response);
        let body = tokio::time::timeout(
            Duration::from_secs(3),
            axum::body::to_bytes(response.into_body(), usize::MAX),
        )
        .await
        .unwrap()
        .unwrap();
        let body = String::from_utf8_lossy(&body).to_string();

        assert!(
            body.contains("upstream request timed out"),
            "body was {body}"
        );
        assert_eq!(outcome.outcome(), StreamOutcome::TimedOut);
        server.abort();
    }

    #[tokio::test]
    async fn a_completed_stream_ends_as_completed() {
        let provider = Router::new()
            .route("/v1/chat/completions", post(completing_stream_response))
            .route("/chat/completions", post(completing_stream_response));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
        let registry = crate::agent::test_registry(&format!("http://{address}/v1"));
        let app = router(registry, None, RequestLimits::default());
        let response = app.oneshot(stream_request()).await.unwrap();
        let outcome = outcome_handle(&response);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body).to_string();

        assert!(
            body.contains("\"finish_reason\":\"stop\""),
            "body was {body}"
        );
        assert!(body.contains("[DONE]"), "body was {body}");
        assert_eq!(
            outcome.outcome(),
            StreamOutcome::Completed,
            "body was {body}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_truncated_upstream_stream_is_reported_and_ends_as_an_error() {
        let provider = Router::new()
            .route("/v1/chat/completions", post(truncated_stream_response))
            .route("/chat/completions", post(truncated_stream_response));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
        let registry = crate::agent::test_registry(&format!("http://{address}/v1"));
        let app = router(registry, None, RequestLimits::default());
        let response = app.oneshot(stream_request()).await.unwrap();
        let outcome = outcome_handle(&response);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body).to_string();

        assert!(
            body.contains("\"error\""),
            "a client must not be told a truncated answer finished; body was {body}"
        );
        assert_eq!(
            outcome.outcome(),
            StreamOutcome::UpstreamError,
            "body was {body}"
        );
        server.abort();
    }

    async fn completing_stream_response() -> axum::response::Response {
        let body = [
            "data: {\"id\":\"one\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"mock \"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"one\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answer\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"one\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n",
            "data: [DONE]\n\n",
        ]
        .concat();
        axum::response::Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(body))
            .unwrap()
    }

    async fn truncated_stream_response() -> axum::response::Response {
        // Content, then the connection ends: no finish_reason, no usage, no [DONE].
        let body = "data: {\"id\":\"cut\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"half an ans\"},\"finish_reason\":null}]}\n\n";
        axum::response::Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from(body))
            .unwrap()
    }

    async fn slow_stream_response() -> axum::response::Response {
        let chunk_stream = stream::unfold(0usize, |index| async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let chunk = "data: {\"id\":\"slow-drip\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"mock-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"},\"finish_reason\":null}]}\n\n";
            Some((
                Ok::<axum::body::Bytes, Infallible>(axum::body::Bytes::from_static(
                    chunk.as_bytes(),
                )),
                index + 1,
            ))
        });
        axum::response::Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(chunk_stream))
            .unwrap()
    }
}
