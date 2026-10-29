# Real-model end-to-end: a model drives an MCP tool

**Status: verified on 29-Oct-2026.** This closes the open item "model-driven tool invocation is
still unproven end to end". It is a recorded run, not a claim — the log lines below are copied
from the process output of the run described here.

## Why this document exists

Two different things can be "unproven", and they need different evidence:

| Question | How it is answered | Where |
|---|---|---|
| Does the gateway enforce `allowed_tools` and log the refusal? | Deterministic unit tests, no model | `src/agent.rs`, `agent::tests` |
| Will a model actually drive a permitted tool, end to end? | A real model against a real MCP server, run once and recorded | this document |

The second cannot be a unit test. A model's choice of tool is not stable across runs or models,
so asserting on it in CI would produce a flaky security test. It is recorded here instead, with
the exact reproduction so anyone can re-run it.

## What was run

| Component | What |
|---|---|
| Riggate | `v0.2.0` + the test commit `daab517`, debug build, `darwin arm64` |
| Model | `qwen2.5:7b` served by Ollama on `127.0.0.1:11434` — local, no egress |
| MCP server | `mcp-server-time` (Python, via `uvx`) bridged from stdio to Streamable HTTP by `supergateway`, on `127.0.0.1:8931/mcp` |
| Allowlist | `allowed_tools = ["get_current_time"]` — the server also advertises `convert_time`, so a denial happens in the same boot |

The MCP server advertising two tools and the allowlist naming one is deliberate: it makes the
admission and the refusal happen on a live server rather than in a fixture.

## Reproduction

```bash
# 1. A real MCP server over Streamable HTTP (bridging stdio -> http)
npx -y supergateway --stdio "uvx mcp-server-time" \
  --outputTransport streamableHttp --port 8931 --host 127.0.0.1

# 2. Riggate, configured with one permitted tool
cat > /tmp/riggate-e2e.toml <<'TOML'
bind = "127.0.0.1:8099"
[[agents]]
name = "local"
provider = "ollama"
model = "qwen2.5:7b"
preamble = "Use the tools you are given when they answer the question. Keep answers short."
max_turns = 6

[[agents.mcp_servers]]
url = "http://127.0.0.1:8931/mcp"
allowed_tools = ["get_current_time"]
TOML

RUST_LOG=info riggate serve --config /tmp/riggate-e2e.toml

# 3. Ask something only the tool can answer
curl -s http://127.0.0.1:8099/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"local","messages":[{"role":"user","content":"What is the current time in Asia/Kolkata? Use your tool."}]}'
```

## What the run produced

The response the caller received (the time is not something the model knew — it is the tool's
output, routed back through the model):

```json
{
  "choices": [{
    "message": {
      "role": "assistant",
      "content": "The current time in Asia/Kolkata is 09:10:23 AM (Thursday, October 29, 2026) and Daylight Saving Time is not in effect."
    },
    "finish_reason": "stop"
  }],
  "usage": {"prompt_tokens": 495, "completion_tokens": 66, "total_tokens": 561}
}
```

The log, which is the part that matters — admission, refusal, execution, and the turn count:

```
INFO riggate::agent: MCP tool policy evaluated agent=local server=http://127.0.0.1:8931/mcp tool=get_current_time allowed=true
INFO riggate::agent: MCP tool policy evaluated agent=local server=http://127.0.0.1:8931/mcp tool=convert_time allowed=false
INFO riggate: riggate listening bind=127.0.0.1:8099 agents=1
INFO chat.request{request_id=chatcmpl-18e2e306184ab1c0 agent="local"}: rig_agent::agent::engine: Current conversation Turns: 1/6
INFO chat.request{...}:execute_tool{gen_ai.tool.name="get_current_time" ...}: riggate::agent: MCP tool invocation started agent=local server=http://127.0.0.1:8931/mcp tool=get_current_time
INFO chat.request{...}:execute_tool{...}: riggate::agent: MCP tool invocation finished agent=local server=http://127.0.0.1:8931/mcp tool=get_current_time succeeded=true
INFO chat.request{...}: rig_agent::agent::engine: Current conversation Turns: 2/6
INFO chat.request{...}: rig_agent::agent::engine: Agent run finished turn=2 max_turns=6
INFO chat.request{...status=200 outcome="completed" duration_ms=8137}: riggate::routes: chat request completed
```

Four things this establishes, each from a log line rather than an inference:

1. **Admission works against a real server.** `tool=get_current_time allowed=true`.
2. **The refusal is real and greppable.** `tool=convert_time allowed=false` — the same server, the
   same boot, a tool the operator did not name, refused and named.
3. **The model drove the call.** `execute_tool{gen_ai.tool.name="get_current_time"}` followed by
   `MCP tool invocation started` / `finished ... succeeded=true`.
4. **The round trip completed.** Two model turns (`Turns: 1/6` then `2/6`), `finish_reason: stop`,
   and a final answer containing the tool's output.

Total request duration: 8.1 s on a 7B model on a laptop, tool call included.

## What this does not prove

- **Tool-selection reliability.** One model, one prompt, one run. A model can decline to use an
  available tool, and `qwen2.5:7b` is not the model a bank would deploy. This proves the mechanism,
  not a success rate.
- **MCP bearer-token auth.** The test server was unauthenticated. `bearer_token_env` is covered by
  unit tests only.
- **Any transport other than Streamable HTTP.** stdio is still on the backlog.
- **Nothing here is in CI.** It needs a running model and a running MCP server, so it is a manual
  check. The deterministic halves are the unit tests.

## Re-verified against this run

`cargo test` — 51 tests, three consecutive runs, no flakes. `cargo clippy --all-targets -- -D
warnings` clean. `cargo fmt --all -- --check` clean.
