# Design decisions

Short record of the choices that shaped v0.1, and the alternatives rejected.

## A single binary, not a deployment

The target user is a platform engineer inside a security perimeter. Every artifact they
must install is a review, a ticket, and a quarter of latency. A compiled binary with no
runtime collapses that: the whole install is `scp` plus a systemd unit.

Rejected: shipping as a container image. It helps the cloud case and not the air-gapped
one, where a registry and an orchestrator are exactly what is missing.

## Binds to loopback by default

`bind` defaults to `127.0.0.1:8080`. Exposure is a deliberate act by the operator, not a
default we chose for them. Most "it leaked to the internet" incidents are defaults.

## OpenAI's HTTP shape, not our own

Clients, SDKs, gateways, and eval harnesses already speak `/v1/chat/completions`. Speaking
it means zero integration work to try `riggate`, and it lets an existing OpenAI client
keep working when the endpoint underneath changes to a local model.

Cost: we inherit a specification we do not control, including fields we ignore.

## Providers configured by environment, not by file

API keys never belong in a config file that gets committed. `rig`'s clients read
`OPENAI_API_KEY` / `OPENAI_BASE_URL` / `OLLAMA_API_BASE_URL` directly, so the config file
holds only names, models, and prompts.

## Errors carry meaning

- unknown model → `404`, with the list endpoint named in the message
- malformed request → `400`, with the supported roles named
- upstream failure → `502` with the provider's message intact

Once a stream has started, an upstream failure is sent as an SSE error event because the
HTTP status can no longer be changed.

## Requests carry their own conversation history

The server does not keep user conversations in memory. Each OpenAI-compatible request
supplies its previous system, user, and assistant text messages; the server passes those to
Rig as history and runs the latest user message. This keeps the binary stateless and lets
clients own conversation storage.

## Streaming follows Rig's agent stream

`stream: true` maps Rig's text events into OpenAI-compatible SSE chunks. Tool and lifecycle
events are not exposed in the HTTP stream yet; the final response and `[DONE]` marker follow
the text chunks.

## Tools come from MCP servers

An agent may configure Streamable HTTP MCP endpoints. At startup Riggate connects each
endpoint, registers its tools with Rig, and holds the connection for the agent's lifetime.
`max_turns` bounds the model calls made by a request, including tool follow-ups. This makes
tools configurable without compiling tool implementations into the server binary.

## Tools are denied unless named

`allowed_tools` is an allowlist with a deny-by-default posture. An omitted field registers no
tools. `[]` is the same. `["*"]` is the only way to grant everything. Mixing `*` with names is
rejected, because it hides which decision was actually made.

Rejected: granting every discovered tool when the field is absent. That reads as convenient and
is the exact thing a security review looks for — a capability granted by omission. The gateway
fails closed everywhere else (loopback bind, API key required off loopback, unknown fields
rejected when strict), and a third-party MCP server adding a tool should not silently widen
what an agent can do.

The cost is discoverability, so every tool decision is logged with the agent, the tool name and
the verdict. The operator starts denied, reads the log for the real names, then names what the
agent may use.

For non-streaming requests, failures use HTTP status codes rather than returning `200` with
an error string inside, which would be untestable by monitoring.

## Agents built once, at boot

Every agent is constructed during startup and stored in an `Arc<HashMap<..>>`. Requests
borrow them. This keeps request latency free of client construction and, more importantly,
fails fast: a missing key is a crash at boot with a clear message, not a 500 under load.

## A stream's status is not its outcome

Once the first token is written the response status is `200` and cannot change. A log that
records only the status cannot distinguish a stream that finished from one that failed upstream,
timed out, or was abandoned by the client — in the very line an operator would use to show
nothing failed. So the guard records `outcome` beside `status`: `completed`, `client_closed`,
`upstream_error` or `timed_out`, and logs at `WARN` for everything but the first.

The same reasoning puts the request id on the wire as `X-Request-Id`, equal to the `id` in the
body and the `request_id` in the log line. An identity that exists only inside the server turns
every support question into a time-range search.

Shutdown is the same problem in the other direction. `SIGTERM` is what a service manager sends,
so handling only `SIGINT` means every restart kills streams mid-answer. The listener stops
accepting, open requests get `shutdown_grace_seconds`, and the process exits with a warning if
they outlive it. Waiting forever is not the alternative to ignoring the signal: it is a
`SIGKILL` with less explanation.

## What Rig gives us

`rig` supplies the provider clients, the agent loop, typed generation options, and the
tool traits. Using it is why this crate is 600 lines rather than 6,000. Rig's `Model`
abstraction is what makes "swap the provider" a config change.
