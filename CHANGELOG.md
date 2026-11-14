# Changelog

## 0.5.0 — 2026-11-14

### Added

- Stdio MCP servers: `command`, `args`, `working_dir`, `env` and `inherit_env` in
  `[[agents.mcp_servers]]`, as an alternative to `url`. The child starts with an empty
  environment and receives only what `env` and `inherit_env` name, so provider and gateway keys
  never leak into it. The config file now names programs the gateway runs; see the README for
  the trust consequences.
- Supervision for stdio children: restart with exponential backoff (1 s to 30 s), stderr
  forwarded into the structured log, a staged stop (stdin close, SIGTERM, SIGKILL) and a kill of
  the whole process group so launchers such as `npx` or `uvx` cannot leave orphans. Exit is
  detected from the process itself, so a grandchild holding stdout open does not hide a crash.
- Per-agent `max_concurrent_requests` and `request_timeout_seconds`. They can only lower the
  server-wide values; a higher value is rejected at startup. A request needs a server slot and an
  agent slot, and the `503` names which one was full.
- HTTP MCP servers are reconnected with the same backoff when their connection closes, and a
  heartbeat (`tools/list`) detects a server that vanishes. `heartbeat_interval_seconds` (default
  15, `0` turns it off) and `heartbeat_timeout_seconds` (default 5) tune it per server.

### Changed

- `/readyz` now reports real state. It returns `200` only while every configured MCP server is
  connected, and `503` otherwise or once shutdown begins, with a body such as
  `{"status":"ready","mcp_servers":{"total":2,"ready":2}}`. It was a fixed "ready" before, so
  orchestrators that probe it will now see it fail while an MCP server is down. It reports counts
  only; the log names the server. Providers are still not probed.
- Riggate no longer exits while a stdio child it started is running: children are stopped after
  in-flight requests drain.
- Each HTTP MCP server now receives a `tools/list` every 15 s by default (see above).

### Fixed

- Dependency updates for known vulnerabilities, including `rmcp` 3.9.0 (RUSTSEC-2026-0462).

## 0.4.0 — 2026-11-04

### Added

- `wire = "chat"` for `openai` agents, to talk to OpenAI-compatible servers that implement only
  `/chat/completions` (vLLM, LiteLLM and similar). The default stays the Responses API.
- An `anthropic` provider for Claude models. It reads `ANTHROPIC_API_KEY` and optionally
  `ANTHROPIC_BASE_URL`, and works with MCP tools and streaming like the other providers.
- `scripts/e2e_tools.sh`: a deterministic end-to-end test of the tool path (Riggate, a scripted
  mock model, and a mock MCP server) covering tool calls, streaming, the allowlist, `max_turns`,
  upstream error mapping and audit logs. `scripts/mock_provider.py` now speaks the OpenAI
  Responses, OpenAI Chat Completions and Anthropic Messages APIs and can script tool calls.
- Per-client bearer tokens, optional agent access lists, and end-user attribution for MCP tool audit records.
- Structured tool decision and invocation audit logs with canonical argument hashes; raw tool arguments are not logged.

### Changed

- A non-streaming reply with no text that was not cut off by a token limit is now a `502`
  ("upstream returned an empty answer") instead of a successful empty completion. It usually
  means the upstream speaks a different API than the agent is configured for.

## 0.3.0 — 2026-10-30

### Added

- Configurable JSON log destinations: stdout, stderr, and optional OTLP/HTTP.
- OTLP batch queue tuning through OpenTelemetry SDK environment variables, a warn-only startup
  connectivity check, and bounded flush on graceful shutdown.
- An unauthenticated `/readyz` endpoint for startup readiness.

### Changed

- JSON logs default to stderr. OTLP export remains opt-in and can be combined with a local stream.
