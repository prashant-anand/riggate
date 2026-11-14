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
- Configured OpenAI, OpenAI-compatible, Anthropic, and Ollama providers. Provider credentials and base
  URLs come from environment variables.
- Streamable HTTP and stdio (child process) MCP servers, with per-server tool allowlists and, for
  HTTP, optional bearer credentials.
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
| `GET` | `/readyz` | Readiness: `200` when every configured MCP server is connected, `503` otherwise; does not require authentication |
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
provider = "openai"               # openai, anthropic, or ollama
model = "gpt-5.5"
# wire = "chat"                   # openai only: "responses" (default) or "chat" for servers
#                                 # that only implement /chat/completions (vLLM, LiteLLM, ...)
preamble = "You are a helpful assistant."
max_turns = 10                     # model calls per request, including tool follow-ups
# max_concurrent_requests = 8      # this agent's own cap, within the server-wide one
# request_timeout_seconds = 60     # this agent's own deadline, within the server-wide one

[[agents.mcp_servers]]
url = "http://127.0.0.1:9000/mcp"
# bearer_token_env = "MCP_SEARCH_TOKEN"
# allowed_tools = ["search", "read"]

# Or run an MCP server as a child process instead of connecting to a URL:
# [[agents.mcp_servers]]
# command = "/usr/local/bin/mcp-server-time"
# args = ["--local-timezone", "UTC"]
# working_dir = "/var/lib/riggate"
# env = { LOG_LEVEL = "warn" }      # literal values handed to the child
# inherit_env = ["HOME"]            # gateway variables copied into the child
# allowed_tools = ["get_current_time"]
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

Logs are JSON lines. Set the top-level `log_targets` list to any combination of `"stdout"`,
`"stderr"`, and `"otlp"`; the default is `["stderr"]`. Each selected target receives the same
filtered events. `RUST_LOG` controls filtering (default: `riggate=info`).

To export through OTLP/HTTP, include `"otlp"` and set `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` (or
the general `OTEL_EXPORTER_OTLP_ENDPOINT`). Optional authentication headers use
`OTEL_EXPORTER_OTLP_LOGS_HEADERS` or `OTEL_EXPORTER_OTLP_HEADERS`, in the OpenTelemetry format
`key=value,key2=value2`. Riggate checks TCP connectivity to the collector at startup, waiting up
to three seconds. A failed check is reported to stderr and does not prevent the API from starting.
This check does not validate TLS, credentials, or whether the endpoint accepts OTLP logs.
Exported logs are batched in a bounded in-memory queue; configure it with the OpenTelemetry SDK
variables `OTEL_BLRP_MAX_QUEUE_SIZE`, `OTEL_BLRP_MAX_EXPORT_BATCH_SIZE`, and
`OTEL_BLRP_SCHEDULE_DELAY` when needed. Failed exports are retried. On graceful shutdown, Riggate
waits up to `shutdown_grace_seconds` for the OTLP batch to flush after request draining; records
can still be lost if the queue fills or the process is killed before shutdown completes. Select
`stderr` alongside `otlp` to retain the local log stream as a fallback.

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
| `ANTHROPIC_API_KEY` | Anthropic provider credential |
| `ANTHROPIC_BASE_URL` | Anthropic-compatible endpoint; defaults to the Anthropic API |
| `OLLAMA_API_BASE_URL` | Ollama endpoint; defaults to `http://localhost:11434` |
| MCP token variable | The variable named by an MCP server's `bearer_token_env` |
| `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` | OTLP log endpoint, when `"otlp"` is in `log_targets` |
| `OTEL_EXPORTER_OTLP_LOGS_HEADERS` | OTLP log authentication or routing headers |
| `OTEL_BLRP_MAX_QUEUE_SIZE` | Maximum in-memory OTLP log queue size |
| `OTEL_BLRP_MAX_EXPORT_BATCH_SIZE` | Maximum OTLP logs per export batch |
| `OTEL_BLRP_SCHEDULE_DELAY` | OTLP log batch export interval in milliseconds |

## Who did what

For deployments with multiple callers, configure one token per client. Tokens are read from the
named environment variables at startup. `allowed_agents` is optional; omitting it permits every
configured agent. `RIGGATE_API_KEY` remains supported as the implicit `default` client.

```toml
[[clients]]
id = "claims-app"
name = "Claims application"
token_env = "CLAIMS_APP_TOKEN"
allowed_agents = ["assistant"]
```

Clients can identify the human user with the OpenAI-compatible top-level `user` field or the
`X-Riggate-On-Behalf-Of` header. If neither is supplied, the request is attributed to
`unattributed` and a warning is logged. Each MCP tool call emits a structured audit record; tool
arguments are represented only by their SHA-256 hash. For example:

```json
{"request_id":"chatcmpl-abc123","client_id":"claims-app","client_name":"Claims application","on_behalf_of":"user-42","agent":"assistant","server":"http://mcp.internal/mcp","tool":"lookup_claim","decision":"allowed","outcome":"ok","error_kind":"","duration_ms":18,"args_sha256":"…","message":"MCP tool audit"}
```

### Per-agent limits

An agent may set `max_concurrent_requests` and `request_timeout_seconds` to restrict itself
further than the server-wide values, so one slow or expensive agent cannot take every slot.
Both are limits only: a value above the server-wide one is rejected at startup rather than
silently ignored, and the stricter of the two always applies. A request needs a server-wide slot
and an agent slot; if either is full it gets `503` with `Retry-After: 1`, and the message says
which one. Both are held for the whole run, including a stream. An agent without these keys is
bound only by the server-wide limits.

### Readiness

`/healthz` says the process is alive; `/readyz` says it can do its job. Ready means every
configured MCP server is currently connected, for stdio servers including after a restart, and
the answer changes as servers drop and return. The body is
`{"status":"ready","mcp_servers":{"total":2,"ready":2}}`, with status `503` and `"not_ready"` when
any are down. When shutdown begins it turns `503` with `"shutting_down"` immediately, before
in-flight requests have drained, so a load balancer stops sending traffic first. Because the probe is unauthenticated it reports counts only; the log names the
server that is down. Providers are not probed, since that would spend tokens or depend on the
network; a provider outage shows up as `502` on requests. An HTTP MCP server whose connection
closes is not reconnected, so it stays unready until Riggate restarts.

### Stdio MCP servers

Each server sets exactly one of `url` or `command`. A `command` server is a child process of the
gateway, which makes the config file executable in effect: **it names programs the gateway will
run with the gateway's privileges.** Give it the review and file permissions you would give code,
and keep it writable only by the account that administers the gateway.

- **Environment is scrubbed.** The child starts with an empty environment and receives only the
  literal `env` entries plus the gateway variables named in `inherit_env`. `OPENAI_API_KEY`,
  `RIGGATE_API_KEY` and client token variables are never passed unless you name them. A name in
  both `env` and `inherit_env` is rejected. This includes `PATH`: a bare `command` is looked up
  on the gateway's own `PATH`, but the child does not get one. Launchers that start further
  programs (`npx`, `uvx`) need `inherit_env = ["PATH"]`; an absolute `command` avoids depending
  on the gateway's `PATH`.
- **Readiness.** Riggate waits for the MCP handshake before it starts listening, and refuses to
  start if a stdio server cannot be spawned or initialised, as it does for HTTP servers.
- **Supervision.** If a child exits, Riggate restarts it with exponential backoff (1 s doubling
  to 30 s, reset after a connection stays up for 60 s). While it is down its tools are not
  offered to the model; they return when it reconnects, under the same `allowed_tools` policy.
- **stderr** is logged, one event per line, with `agent`, `server` and `stream="stderr"`, so it
  follows your `log_targets`. Lines longer than 8 KiB are split.
- **Shutdown.** After in-flight requests drain, each child is stopped in stages: its stdin is
  closed and it gets 3 s to exit, then it receives SIGTERM and 3 s more, then SIGKILL. Riggate
  does not exit while a child it started is running.
- **Process groups.** Each child leads its own process group, and the whole group is sent
  SIGKILL when the server stops, crashes or fails its handshake. A launcher such as `npx` or
  `uvx` therefore cannot leave the real server running. Exit is detected from the process
  itself, not only its pipes, so a grandchild holding stdout open does not hide a crash. This
  is Unix only; on other platforms only the process Riggate started is stopped.
- **Config reload** does not exist yet (change the file and restart), so there is no reload
  path to clean up.

HTTP MCP endpoints must be reachable from the Riggate host.

## Built with Rig

Rig supplies the agent runtime and provider clients. Riggate adds configuration, an
OpenAI-compatible HTTP interface, MCP connections, and request controls around it. See
[`docs/DESIGN.md`](docs/DESIGN.md) for the design decisions.

## Roadmap

- Retrieval over a vector store you host.

## Licence

MIT.
