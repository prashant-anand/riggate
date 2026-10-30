# Changelog

## 0.3.0 — 2026-10-30

### Added

- Configurable JSON log destinations: stdout, stderr, and optional OTLP/HTTP.
- OTLP batch queue tuning through OpenTelemetry SDK environment variables, a warn-only startup
  connectivity check, and bounded flush on graceful shutdown.
- An unauthenticated `/readyz` endpoint for startup readiness.

### Changed

- JSON logs default to stderr. OTLP export remains opt-in and can be combined with a local stream.
