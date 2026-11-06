#!/usr/bin/env bash
set -euo pipefail

# Riggate tool-path end-to-end test.
#
# Chain under test, with no real model and no network:
#   curl -> Riggate -> scripted OpenAI mock (mock_provider.py) -> MCP tool calls
#                   -> mock Streamable HTTP MCP server (mock_mcp.py)
#
# The mock model is told what to do by directives in the user message (see mock_provider.py),
# so every run is deterministic. This proves the plumbing and the policy (allowlist, max_turns,
# audit log, error mapping); it says nothing about how a real model behaves. For that see
# docs/real-model-e2e.md.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
ENV_DIR="${REPO_ROOT}/target/e2e-tools"
RIGGATE_PORT="${RIGGATE_E2E_PORT:-18081}"
BASE="http://127.0.0.1:${RIGGATE_PORT}"

if ! command -v cargo &>/dev/null && [ -f "${HOME}/.cargo/bin/cargo" ]; then
    export PATH="${HOME}/.cargo/bin:${PATH}"
fi

if [ "${SKIP_BUILD:-0}" != "1" ]; then
    (cd "${REPO_ROOT}" && cargo build --quiet)
fi
BIN="${REPO_ROOT}/target/debug/riggate"

rm -rf "${ENV_DIR}"
mkdir -p "${ENV_DIR}"
export MOCK_REQUEST_LOG="${ENV_DIR}/upstream-requests.jsonl"
export MOCK_MCP_LOG="${ENV_DIR}/mcp-calls.jsonl"
export MOCK_MCP_TOKEN="mcp-secret-token"
export OPENAI_API_KEY="test-openai-key"
export RIGGATE_API_KEY="e2e-riggate-key"
: > "${MOCK_REQUEST_LOG}"
: > "${MOCK_MCP_LOG}"

PIDS=()
cleanup() {
    for pid in "${PIDS[@]:-}"; do
        [ -n "${pid}" ] && kill "${pid}" 2>/dev/null || true
    done
    wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

start_mock() { # script, outfile
    python3 "${SCRIPT_DIR}/$1" 0 > "$2" 2> "$2.err" &
    PIDS+=($!)
    for _ in {1..50}; do
        [ -s "$2" ] && break
        sleep 0.1
    done
    [ -s "$2" ] || { echo "$1 failed to start" >&2; cat "$2.err" >&2; exit 1; }
    grep PORT= "$2" | cut -d= -f2
}

LLM_PORT=$(start_mock mock_provider.py "${ENV_DIR}/llm.port")
MCP_PORT=$(start_mock mock_mcp.py "${ENV_DIR}/mcp.port")
export OPENAI_BASE_URL="http://127.0.0.1:${LLM_PORT}/v1"
export ANTHROPIC_API_KEY="test-anthropic-key"
export ANTHROPIC_BASE_URL="http://127.0.0.1:${LLM_PORT}"
export MCP_TOKEN="${MOCK_MCP_TOKEN}"

cat > "${ENV_DIR}/riggate.toml" <<EOF
bind = "127.0.0.1:${RIGGATE_PORT}"
log_targets = ["stderr"]
request_timeout_seconds = 3
shutdown_grace_seconds = 1

[[agents]]
name = "tools"
provider = "openai"
model = "mock-model"
preamble = "You are a test agent."
max_turns = 5

[[agents.mcp_servers]]
url = "http://127.0.0.1:${MCP_PORT}/mcp"
bearer_token_env = "MCP_TOKEN"
allowed_tools = ["echo", "add"]

[[agents]]
name = "plain"
provider = "openai"
model = "mock-model"

[[agents]]
name = "chat"
provider = "openai"
wire = "chat"
model = "mock-model"
preamble = "You are a test agent."
max_turns = 5

[[agents.mcp_servers]]
url = "http://127.0.0.1:${MCP_PORT}/mcp"
bearer_token_env = "MCP_TOKEN"
allowed_tools = ["echo", "add"]

[[agents]]
name = "claude"
provider = "anthropic"
model = "claude-sonnet-5-5"
preamble = "You are a test agent."
max_turns = 5

[[agents.mcp_servers]]
url = "http://127.0.0.1:${MCP_PORT}/mcp"
bearer_token_env = "MCP_TOKEN"
allowed_tools = ["echo", "add"]
EOF

"${BIN}" check --config "${ENV_DIR}/riggate.toml" > /dev/null
"${BIN}" serve --config "${ENV_DIR}/riggate.toml" 2> "${ENV_DIR}/riggate.log" &
PIDS+=($!)
for _ in {1..100}; do
    curl -s "${BASE}/readyz" | grep -q ready && break
    sleep 0.1
done
curl -s "${BASE}/readyz" | grep -q ready || { echo "Riggate not ready" >&2; cat "${ENV_DIR}/riggate.log" >&2; exit 1; }
echo "[OK] stack up: riggate :${RIGGATE_PORT}, mock llm :${LLM_PORT}, mock mcp :${MCP_PORT}"

PASSED=0
FAILED=0
check() { # description, command...
    local desc="$1"; shift
    if "$@"; then
        echo "  [PASS] ${desc}"; PASSED=$((PASSED + 1))
    else
        echo "  [FAIL] ${desc}" >&2; FAILED=$((FAILED + 1))
    fi
}

# chat <agent> <prompt> [extra-json-fields] -> writes body to ${ENV_DIR}/body, prints status
chat() {
    local agent="$1" prompt="$2" extra="${3:-}"
    python3 - "$agent" "$prompt" "$extra" > "${ENV_DIR}/req.json" <<'PY'
import json, sys
agent, prompt, extra = sys.argv[1:4]
body = {"model": agent, "messages": [{"role": "user", "content": prompt}]}
if extra:
    body.update(json.loads(extra))
print(json.dumps(body))
PY
    curl -s -o "${ENV_DIR}/body" -w "%{http_code}" "${BASE}/v1/chat/completions" \
        -H "authorization: Bearer ${RIGGATE_API_KEY}" -H 'content-type: application/json' \
        -d @"${ENV_DIR}/req.json"
}
body_has() { grep -q -- "$1" "${ENV_DIR}/body"; }
mcp_calls() { grep -c "\"tool\": \"$1\"" "${MCP_LOG:-${MOCK_MCP_LOG}}" || true; }
log_has() { grep -q -- "$1" "${ENV_DIR}/riggate.log"; }

echo "1. Model-driven tool call (non-streaming)"
STATUS=$(chat tools $'MOCK_TOOL echo {"text":"hi"}')
check "returns 200" [ "${STATUS}" = "200" ]
check "final answer carries the tool result" body_has 'echo:hi'
check "MCP server really received the call" [ "$(mcp_calls echo)" = "1" ]
check "audit log records the allowed decision" log_has 'MCP tool audit decision.*"tool":"echo"'
check "audit log records the invocation as ok" log_has 'MCP tool audit invocation.*"tool":"echo".*"outcome":"ok"'
check "audit log never contains the raw tool arguments" \
    bash -c "! grep -q -e 'echo:hi' -e 'streamed' -e 'again' '${ENV_DIR}/riggate.log'"

echo "2. Parallel tool calls"
STATUS=$(chat tools $'MOCK_TOOL echo {"text":"x"}\nMOCK_TOOL add {"a":2,"b":3}')
check "returns 200" [ "${STATUS}" = "200" ]
check "both results reach the model" body_has '5'
check "add reached the MCP server" [ "$(mcp_calls add)" = "1" ]

echo "3. Streaming with a tool call"
STATUS=$(chat tools $'MOCK_TOOL echo {"text":"streamed"}' '{"stream":true}')
check "returns 200" [ "${STATUS}" = "200" ]
check "SSE stream carries the final text" body_has 'echo:streamed'
check "SSE stream terminates with [DONE]" body_has '\[DONE\]'

echo "4. Allowlist: a tool the model asks for but the operator did not allow"
BEFORE=$(mcp_calls danger)
STATUS=$(chat tools $'MOCK_TOOL danger {}')
AFTER=$(mcp_calls danger)
check "danger never reaches the MCP server" [ "${BEFORE}" = "${AFTER}" ]
check "startup log records the denial" log_has 'danger'
check "danger was never offered to the model" \
    bash -c "! grep -q '\"name\": *\"danger\"' '${MOCK_REQUEST_LOG}'"

echo "5. Upstream failures"
STATUS=$(chat tools $'MOCK_STATUS 500')
check "upstream 500 maps to 502 (got ${STATUS})" [ "${STATUS}" = "502" ]
STATUS=$(chat plain $'MOCK_SLEEP 6')
check "upstream slower than request_timeout_seconds maps to 504 (got ${STATUS})" [ "${STATUS}" = "504" ]

echo "6. max_turns"
BEFORE=$(grep -c '"text": "again"' "${MOCK_MCP_LOG}" || true)
STATUS=$(chat tools $'MOCK_LOOP echo {"text":"again"}')
AFTER=$(grep -c '"text": "again"' "${MOCK_MCP_LOG}" || true)
check "a model that never stops is cut off with 502 (got ${STATUS})" [ "${STATUS}" = "502" ]
check "tool ran exactly max_turns (5) times, then stopped (ran $((AFTER - BEFORE)))" [ "$((AFTER - BEFORE))" = "5" ]

echo "7. Malformed tool arguments"
STATUS=$(chat tools $'MOCK_TOOL add {"a":1}')
check "request completes (got ${STATUS})" [ "${STATUS}" = "200" ]
check "the failed call is audited as an error" log_has 'MCP tool audit invocation.*"tool":"add".*"outcome":"error"'

echo "8. Stateless history is forwarded verbatim"
: > "${MOCK_REQUEST_LOG}"
python3 - > "${ENV_DIR}/req.json" <<'PY'
import json
print(json.dumps({"model": "plain", "messages": [
    {"role": "user", "content": "first question"},
    {"role": "assistant", "content": "first answer"},
    {"role": "user", "content": "second question"},
]}))
PY
STATUS=$(curl -s -o "${ENV_DIR}/body" -w "%{http_code}" "${BASE}/v1/chat/completions" \
    -H "authorization: Bearer ${RIGGATE_API_KEY}" -H 'content-type: application/json' -d @"${ENV_DIR}/req.json")
check "returns 200" [ "${STATUS}" = "200" ]
check "upstream saw the earlier turns" grep -q 'first answer' "${MOCK_REQUEST_LOG}"

echo "9. Anthropic provider (Messages API)"
: > "${MOCK_REQUEST_LOG}"
STATUS=$(chat claude $'MOCK_TOOL echo {"text":"claude-hi"}')
check "returns 200 (got ${STATUS})" [ "${STATUS}" = "200" ]
check "final answer carries the tool result" body_has 'echo:claude-hi'
check "upstream was called on the Messages path" grep -q '"path": "[^"]*/v1/messages"' "${MOCK_REQUEST_LOG}"
check "upstream got the API key in x-api-key" grep -q '"x_api_key": "test-anthropic-key"' "${MOCK_REQUEST_LOG}"
check "upstream got the preamble as the system prompt" grep -q 'You are a test agent' "${MOCK_REQUEST_LOG}"
check "MCP tools were offered upstream" grep -q '"name": "echo"' "${MOCK_REQUEST_LOG}"
check "the tool result went back upstream as a tool_result block" grep -q 'tool_result' "${MOCK_REQUEST_LOG}"
STATUS=$(chat claude $'MOCK_TOOL echo {"text":"claude-stream"}\nMOCK_TOOL add {"a":4,"b":5}' '{"stream":true}')
check "streaming returns 200 (got ${STATUS})" [ "${STATUS}" = "200" ]
check "streamed answer carries both tool results" bash -c "grep -q 'echo:claude-stream' '${ENV_DIR}/body' && grep -q '9' '${ENV_DIR}/body'"
STATUS=$(chat claude $'MOCK_STATUS 500')
check "Anthropic upstream 500 maps to 502 (got ${STATUS})" [ "${STATUS}" = "502" ]

echo "10. OpenAI Chat Completions wire (wire = \"chat\")"
: > "${MOCK_REQUEST_LOG}"
STATUS=$(chat chat $'MOCK_TOOL echo {"text":"via-chat"}')
check "returns 200 (got ${STATUS})" [ "${STATUS}" = "200" ]
check "final answer carries the tool result" body_has 'echo:via-chat'
check "upstream was called on /chat/completions" grep -q '"path": "[^"]*/chat/completions"' "${MOCK_REQUEST_LOG}"
check "upstream was not called on /responses" bash -c "! grep -q '/responses' '${MOCK_REQUEST_LOG}'"
STATUS=$(chat chat $'MOCK_TOOL add {"a":20,"b":22}' '{"stream":true}')
check "streaming returns 200 (got ${STATUS})" [ "${STATUS}" = "200" ]
check "streamed answer carries the tool result" body_has '42'
STATUS=$(chat chat $'MOCK_STATUS 500')
check "Chat upstream 500 maps to 502 (got ${STATUS})" [ "${STATUS}" = "502" ]
: > "${MOCK_REQUEST_LOG}"
STATUS=$(chat tools 'default wire')
check "agents without wire still use /responses" grep -q '"path": "[^"]*/responses"' "${MOCK_REQUEST_LOG}"

echo "11. An upstream that answers 200 with the wrong kind of body"
for agent in tools chat claude; do
    STATUS=$(chat "${agent}" $'MOCK_GARBAGE')
    check "${agent}: not reported as a successful empty answer (got ${STATUS})" [ "${STATUS}" != "200" ]
done

for agent in tools claude; do
    STATUS=$(chat "${agent}" $'MOCK_CHATBODY')
    check "${agent}: a valid body of the wrong dialect is not an empty success (got ${STATUS}; body: $(head -c 120 "${ENV_DIR}/body"))" [ "${STATUS}" != "200" ]
done

echo ""
echo "=========================================="
echo "Tool e2e summary: ${PASSED} passed, ${FAILED} failed"
echo "Artifacts: ${ENV_DIR}"
echo "=========================================="
[ "${FAILED}" -eq 0 ]
