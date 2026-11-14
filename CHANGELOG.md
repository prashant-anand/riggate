# Changelog

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
