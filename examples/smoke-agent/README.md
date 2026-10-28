# Smoke test: one request, no network egress

Proves the whole path — HTTP request, agent run, provider, response — against a local model.
Nothing leaves the machine. Takes about a minute once the model is pulled.

## 1. A model to talk to

```bash
ollama serve                  # if it is not already running
ollama pull gemma4:12b        # 7.7 GB, once
```

Check it answers on its own:

```bash
curl http://localhost:11434/api/tags | head -c 200
```

## 2. A config with one local agent

Save this as `smoke.toml`. It has a single Ollama agent, so no hosted credential is needed.

```toml
bind = "127.0.0.1:8080"

[[agents]]
name = "local"
provider = "ollama"
model = "gemma4:12b"
preamble = "Answer in one sentence."
max_turns = 4
```

Confirm it parses before starting anything:

```bash
riggate check --config smoke.toml
# config ok — 1 agent(s)
#   local -> gemma4:12b (Ollama), max_turns=4, mcp_servers=0
```

## 3. Start the server

```bash
RIGGATE_API_KEY=smoke-test riggate serve --config smoke.toml
# INFO riggate: riggate listening bind=127.0.0.1:8080 agents=1
```

Leave it running and use a second terminal.

## 4. Ask it something

```bash
curl -s http://127.0.0.1:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -H 'authorization: Bearer smoke-test' \
  -d '{"model":"local","messages":[{"role":"user","content":"What is the capital of France?"}]}' \
  | python3 -m json.tool
```

The response is OpenAI-shaped: `choices[0].message.content` holds the answer, `usage` holds the
token counts.

## 5. Streaming

```bash
curl -sN http://127.0.0.1:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -H 'authorization: Bearer smoke-test' \
  -d '{"model":"local","stream":true,"messages":[{"role":"user","content":"Count to five."}]}'
```

Chunks arrive as `data:` events and end with `data: [DONE]`.

## 6. Conversation history is the client's job

Riggate keeps no state. Send the earlier turns back on every call:

```bash
curl -s http://127.0.0.1:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -H 'authorization: Bearer smoke-test' \
  -d '{
    "model": "local",
    "messages": [
      {"role": "user", "content": "My name is Prashant."},
      {"role": "assistant", "content": "Hello Prashant."},
      {"role": "user", "content": "What is my name?"}
    ]
  }' | python3 -m json.tool
```

## When it does not work

| Symptom | Cause |
|---|---|
| `an openai agent needs OPENAI_API_KEY` at startup | the config has an `openai` agent; comment it out or export the key |
| `connecting MCP server ... failed` | an MCP endpoint is configured but not running; comment that block out |
| `upstream error: ...` with `502` | Ollama is not running, or the model name is not pulled |
| `unknown model 'local'` with `404` | the agent name in the request does not match `name` in the config |
| `401` | `RIGGATE_API_KEY` is set but the request has no `authorization: Bearer ...` header |
