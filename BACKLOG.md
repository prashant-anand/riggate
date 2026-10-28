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

## MCP transports

Today only Streamable HTTP is supported. The transport itself is not the work: rmcp ships `transport-child-process`, and the client handler takes any transport. The work is the config surface, the supervision, and the trust boundary a local child process implies.

- [ ] Accept stdio MCP servers: `command`, `args`, and `working_dir` in `[[agents.mcp_servers]]`, as an alternative to `url`.
- [ ] Scrub the child environment. `Command::new` inherits the gateway's variables by default, so a child would read `OPENAI_API_KEY` and `RIGGATE_API_KEY` unless an explicit allowlist is passed. Fail closed: inherit nothing that is not named.
- [ ] Supervise stdio children: restart with backoff, stderr into tracing, readiness before the first request is served.
- [ ] Terminate children on shutdown and on config reload. No orphans when the gateway exits.
- [ ] Apply the same tool policy to both transports: `allowed_tools` filtering and the tool-policy log lines must not depend on how the server is reached.
- [ ] Test stdio against a fake MCP child process, not only the HTTP mock provider.
- [ ] Document the security consequence: the config file names programs to execute, so it needs the review and file permissions that code gets.
