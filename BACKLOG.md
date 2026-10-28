# Backlog

Candidate improvements for Riggate. This list captures ideas for discussion and prioritization; inclusion does not imply a release commitment.

## Monitoring and operations

- [ ] Add per-target health checks for configured agents, including reachability, last success, latency, and recent errors.
- [ ] Add readiness reporting that distinguishes Riggate being alive from configured providers being ready.
- [ ] Expose operational metrics such as request count, latency, failures, concurrency, and token usage when available.

## Routing and controls

- [ ] Add configurable fallback targets for provider or model failures.
- [ ] Support per-agent concurrency and timeout limits in addition to server-wide limits.

## Providers

- [ ] Evaluate and add support for more providers available through Rig, guided by user demand and configuration needs.
