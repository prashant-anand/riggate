use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

#[tokio::main]
async fn main() -> Result<()> {
    let base_url = std::env::var("RIGGATE_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string())
        .trim_end_matches('/')
        .to_string();
    let model = std::env::var("RIGGATE_MODEL").unwrap_or_else(|_| "demo".to_string());
    let prompt = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    let prompt = if prompt.is_empty() {
        "Reply with the word ready.".to_string()
    } else {
        prompt
    };
    let api_key = std::env::var("RIGGATE_API_KEY").ok();
    let client = reqwest::Client::new();

    client
        .get(format!("{base_url}/healthz"))
        .send()
        .await
        .context("connecting to Riggate health endpoint")?
        .error_for_status()
        .context("Riggate health check failed")?;

    let models_request = client.get(format!("{base_url}/v1/models"));
    let models_request = with_bearer(models_request, api_key.as_deref());
    let models_response = models_request
        .send()
        .await
        .context("requesting the Riggate model list")?;
    let models_response = checked_response(models_response).await?;
    let models: Value = models_response
        .json()
        .await
        .context("decoding the Riggate model list")?;
    let model_is_available = models["data"]
        .as_array()
        .is_some_and(|entries| entries.iter().any(|entry| entry["id"] == model));
    if !model_is_available {
        bail!("model '{model}' is not configured in Riggate; available models: {models}");
    }

    let chat_request = client
        .post(format!("{base_url}/v1/chat/completions"))
        .json(&json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}]
        }));
    let chat_request = with_bearer(chat_request, api_key.as_deref());
    let response = chat_request
        .send()
        .await
        .context("sending a chat completion request to Riggate")?;
    let response = checked_response(response).await?;
    let completion: Value = response
        .json()
        .await
        .context("decoding the chat completion response")?;
    let answer = completion["choices"][0]["message"]["content"]
        .as_str()
        .context("Riggate response did not contain assistant text")?;

    println!("{answer}");
    Ok(())
}

fn with_bearer(request: reqwest::RequestBuilder, api_key: Option<&str>) -> reqwest::RequestBuilder {
    match api_key {
        Some(key) => request.bearer_auth(key),
        None => request,
    }
}

async fn checked_response(response: reqwest::Response) -> Result<reqwest::Response> {
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    bail!("Riggate returned {status}: {body}");
}
