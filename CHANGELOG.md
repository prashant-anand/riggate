# Changelog

## 0.4.0 — 2026-11-04

### Added

- Per-client bearer tokens, optional agent access lists, and end-user attribution for MCP tool audit records.
- Structured tool decision and invocation audit logs with canonical argument hashes; raw tool arguments are not logged.

## 0.3.0 — 2026-10-30

### Added

- Configurable JSON log destinations: stdout, stderr, and optional OTLP/HTTP.
- OTLP batch queue tuning through OpenTelemetry SDK environment variables, a warn-only startup
  connectivity check, and bounded flush on graceful shutdown.
- An unauthenticated `/readyz` endpoint for startup readiness.

### Changed

- JSON logs default to stderr. OTLP export remains opt-in and can be combined with a local stream.
