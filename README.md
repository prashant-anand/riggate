# riggate

Riggate runs [Rig](https://rig.rs) agents behind an OpenAI-compatible HTTP API. It is a
single Rust binary that you can run inside your own network with OpenAI, an OpenAI-compatible
provider, or Ollama.

## Measured

Both are stripped, thin-LTO builds from the release profile below.

| Build | Size (MiB) |
|---|---:|
| `aarch64-apple-darwin`, rustc 1.99.1 | **9.9** |
| `x86_64-unknown-linux-musl`, rustc 1.99.1 | **12.7** |

The Linux binary is `static-pie linked` and names no shared libraries. The macOS binary links
five system frameworks and no OpenSSL.

## Quickstart

Install Riggate and copy the example configuration:

```bash
cargo install riggate                    # from crates.io
# from a checkout instead: cargo install --path .
cp examples/riggate.toml riggate.toml
riggate check --config riggate.toml
```

The example config includes an OpenAI agent and an Ollama agent, so set credentials for the
providers you configure. The OpenAI agent needs `OPENAI_API_KEY`. Ollama defaults to
`http://localhost:11434`; set `OLLAMA_API_BASE_URL` if your Ollama server runs elsewhere. Make
sure Ollama is running and has the configured model available before sending requests to it.

```bash
export OPENAI_API_KEY="your-provider-key"
export RIGGATE_API_KEY="$(openssl rand -hex 32)"
riggate serve --config riggate.toml
```

The server listens on `127.0.0.1:8080`. The API key is optional on loopback, but when set it
protects `/v1/models` and `/v1/chat/completions`. Any non-loopback bind requires
`RIGGATE_API_KEY`.

Send a chat completion request:

```bash
curl http://127.0.0.1:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -H "authorization: Bearer ${RIGGATE_API_KEY}" \
  -d '{"model":"local","messages":[{"role":"user","content":"What is the capital of France?"}]}'
```

The `model` value is the configured agent name. Riggate returns an OpenAI-shaped chat
completion response.

To test a local agent end to end, see the [smoke-agent example](examples/smoke-agent/README.md).

## What it supports

- OpenAI Chat Completions requests at `/v1/chat/completions`, including server-sent event
  streaming.
- Configured OpenAI, OpenAI-compatible, and Ollama providers. Provider credentials and base
  URLs come from environment variables.
- Streamable HTTP MCP servers, with optional bearer credentials and per-server tool allowlists.
- Request body limits, an upstream timeout, and a configurable limit on concurrent agent runs.
- Stateless conversations: clients send the conversation history with each request.

Supported generation options are `temperature`, `top_p`, `max_tokens`,
`max_completion_tokens`, `stop`, `frequency_penalty`, and `presence_penalty`. The two token
limit fields cannot be used together. Unknown request fields are accepted and ignored by
default; set `strict_requests = true` to reject them. Invalid option values return a clear
`400` error.

## HTTP API

| Method | Path | Description |
|---|---|---|
| `GET` | `/healthz` | Liveness check; does not require authentication |
| `GET` | `/v1/models` | Lists configured agents |
| `POST` | `/v1/chat/completions` | Runs an agent |

Requests accept text messages with `system`, `user`, or `assistant` roles. The final message
must be from the user. Message content may be a string or an array of text parts; image and
other non-text parts are not supported. Errors use HTTP status codes: unknown models return
`404`, invalid requests return `400`, oversized bodies return `413`, upstream failures return `502`, and
requests rejected at the concurrency limit return `503`. Upstream timeouts return `504`; if a
stream has started, errors are sent in an SSE event.

Every response carries an `X-Request-Id` header, and it is the same value as the `id` in the
body and the `request_id` in the log line for that request. Quote it when reporting a problem
and the log line can be found without a time range.

`finish_reason` is whatever the provider reported. When a provider reports nothing, `stop` is
returned unless the answer landed exactly on a `max_tokens` limit you set, which is reported as
`length`. Token counts are not used to overrule a reported reason, so an answer that happens to
reach the cap is not labelled truncated.

Riggate serves plaintext HTTP and does not terminate TLS. For remote access, put it behind a
TLS-enabled ingress or reverse proxy; do not expose the plaintext listener directly to an
untrusted network.

## Configuration

```toml
bind = "127.0.0.1:8080"
max_request_bytes = 1048576
max_concurrent_requests = 64
request_timeout_seconds = 120
shutdown_grace_seconds = 10
strict_requests = false

[[agents]]
name = "assistant"                # clients use this as the model name
provider = "openai"               # openai or ollama
model = "gpt-5.5"
preamble = "You are a helpful assistant."
max_turns = 10                     # model calls per request, including tool follow-ups

[[agents.mcp_servers]]
url = "http://127.0.0.1:9000/mcp"
# bearer_token_env = "MCP_SEARCH_TOKEN"
# allowed_tools = ["search", "read"]
```

The top-level request controls default to a 1 MiB body limit, 64 concurrent agent runs, and a
120-second upstream timeout. Unknown request fields are ignored by default; set
`strict_requests = true` to restore rejection of unknown fields.

On `SIGINT` or `SIGTERM` the listener stops accepting new requests and open ones get up to
`shutdown_grace_seconds` (default 10) to finish, so a stream mid-answer is not cut off by a
restart. If requests are still open when the grace period ends, the process exits and logs a
warning naming the period it waited.

Each request logs one line with `request_id`, `agent`, `status`, `outcome`, `duration_ms` and
token counts. `outcome` is `completed`, `rejected`, `upstream_error`, `timed_out` or
`client_closed`; the last three are the failure modes a bare `status` hides, because a stream
that broke after its first token was already sent as `200`.

MCP tools are denied unless they are named. Omitting `allowed_tools`, or setting `[]`,
registers no tools from that server. Set `allowed_tools = ["*"]` to register every discovered
tool. Every decision is logged at startup with the agent, the MCP server URL, the tool name, and
whether it was allowed, so the log is how you find the names to allow. The policy is applied
again when the server refreshes its tool list.

Set `bearer_token_env` to the name of an environment variable containing the MCP server's
bearer token. Riggate reads the value when it starts and does not store credentials in the TOML
file.

| Environment variable | Purpose |
|---|---|
| `RIGGATE_API_KEY` | Protect API routes and allow non-loopback binds |
| `OPENAI_API_KEY` | OpenAI provider credential |
| `OPENAI_BASE_URL` | OpenAI-compatible endpoint, such as Azure OpenAI or vLLM |
| `OLLAMA_API_BASE_URL` | Ollama endpoint; defaults to `http://localhost:11434` |
| MCP token variable | The variable named by an MCP server's `bearer_token_env` |

MCP endpoints must be reachable from the Riggate host. Riggate currently supports Streamable
HTTP transport; stdio transport is not implemented.

## Built with Rig

Rig supplies the agent runtime and provider clients. Riggate adds configuration, an
OpenAI-compatible HTTP interface, MCP connections, and request controls around it. See
[`docs/DESIGN.md`](docs/DESIGN.md) for the design decisions.

## Roadmap

- Retrieval over a vector store you host.

## Licence

MIT.
