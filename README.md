# sallyport

**A single-binary agent server for air-gapped networks.** Bring your own model.

A sally port is the one controlled gate into a fortress. That is this: a small, auditable
door through which a network lets an AI agent work — and nothing else crosses.

```
$ sallyport serve
2026-10-28T11:22:04Z  INFO sallyport: sallyport listening bind=127.0.0.1:8080 agents=1
```

## Why

Most agent infrastructure assumes the cloud. For a bank, a hospital, or a captive centre,
that assumption ends the project at the security review: the agent runs in someone else's
datacentre, or it doesn't run.

`sallyport` takes the opposite position.

- **One file.** No interpreter, no container runtime, no package manager. Copy it, run it.
- **Your network.** Binds to loopback by default. Egress goes only where you point it.
- **Your model.** OpenAI, Azure OpenAI, any OpenAI-compatible endpoint, or Ollama inside
  the building. With a local model it runs with the WAN unplugged.
- **An API your tools already speak.** OpenAI's `/v1/chat/completions` shape, so existing
  SDKs and clients work without a custom integration.

## Measured

| | |
|---|---|
| Release binary | **13 MB**, self-contained |
| Runtime dependencies | none — no Python, no JVM, no containers |
| OS libraries linked (macOS) | 5 system frameworks, no OpenSSL (rustls) |
| Answers from a local model | verified with `ollama` + `gemma4:12b`, no network egress |

## Quickstart

```bash
cargo install --path .
cp examples/sallyport.toml sallyport.toml
sallyport check --config sallyport.toml
sallyport serve --config sallyport.toml
```

Then, from any OpenAI-compatible client:

```bash
curl -s http://127.0.0.1:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"local","messages":[{"role":"user","content":"What is the capital of France?"}]}'
```

```json
{"id":"chatcmpl-18e2ad9c34277be8","object":"chat.completion","model":"local",
 "choices":[{"index":0,"message":{"role":"assistant","content":"Paris."},"finish_reason":"stop"}],
 "usage":{"prompt_tokens":32,"completion_tokens":196,"total_tokens":228}}
```

## Endpoints

| Method | Path | Purpose |
|---|---|---|
| `GET` | `/healthz` | liveness probe |
| `GET` | `/v1/models` | configured agents, OpenAI shape |
| `POST` | `/v1/chat/completions` | run one agent |

Errors are honest: an unknown model is `404`, streaming is `501`, and an upstream provider
failure is `502` carrying the provider's own message.

## Configuration

```toml
bind = "127.0.0.1:8080"          # loopback by default; exposure is opt-in

[[agents]]
name = "assistant"                # the value clients send as "model"
provider = "openai"               # openai | ollama
model = "gpt-5.5"
preamble = "You are a helpful assistant."
```

Provider endpoints and credentials come from the environment, never the config file:

| Variable | Used by |
|---|---|
| `OPENAI_API_KEY` | `provider = "openai"` |
| `OPENAI_BASE_URL` | Azure OpenAI, vLLM, or an internal gateway |
| `OLLAMA_API_BASE_URL` | `provider = "ollama"` (optional; defaults to localhost) |

## Status

`v0.1` — single-turn agents over an OpenAI-compatible HTTP surface. Deliberately small.

Not yet implemented, in order of intent:

1. **Streaming** (`stream: true`) — currently `501`
2. **Multi-turn** — conversation history is not carried between requests
3. **Tools** — typed function calling via `#[rig::rig_tool]`
4. **MCP** — expose MCP servers as tool sources (Rig's `rmcp` feature)
5. **Retrieval** — RAG over a vector store you host

This is not a hosted platform, an orchestrator, or a multi-agent framework. It is one
binary that runs one or more configured agents behind a boring HTTP API.

## Built on

[Rig](https://rig.rs) — a Rust agent framework with one API over 20+ providers — plus
`axum` and `tokio`. See [`docs/DESIGN.md`](docs/DESIGN.md) for why the shape is what it is.

## Licence

MIT.
