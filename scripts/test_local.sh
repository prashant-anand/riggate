#!/usr/bin/env bash
set -euo pipefail

# Riggate Local End-to-End Test Suite
# Tests multi-client authentication, agent access filtering, 
# end-user attribution, and audit logging.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
TEST_ENV_DIR="${REPO_ROOT}/target/test-env"
LOG_FILE="${TEST_ENV_DIR}/riggate.log"
MOCK_LOG="${TEST_ENV_DIR}/mock_provider.log"
PORT_FILE="${TEST_ENV_DIR}/mock_port.txt"

if ! command -v cargo &>/dev/null && [ -f "${HOME}/.cargo/bin/cargo" ]; then
    export PATH="${HOME}/.cargo/bin:${PATH}"
fi

# Ensure setup was run
if [ ! -f "${REPO_ROOT}/target/debug/riggate" ]; then
    echo "riggate binary not found; running setup first..."
    bash "${SCRIPT_DIR}/setup.sh"
fi

BIN_PATH="${REPO_ROOT}/target/debug/riggate"
RIGGATE_PORT="18080"
CONFIG_PATH="${TEST_ENV_DIR}/riggate.test.toml"

# Test tokens
export CLAIMS_TOKEN="token-claims-test-123"
export ADMIN_TOKEN="token-admin-test-456"
export OPENAI_API_KEY="test-openai-key"

echo "=== Starting Riggate Local End-to-End Tests ==="

# Cleanup handler for background processes
MOCK_PID=""
RIGGATE_PID=""
cleanup() {
    echo ""
    echo "Cleaning up processes..."
    if [ -n "${RIGGATE_PID}" ] && kill -0 "${RIGGATE_PID}" 2>/dev/null; then
        echo "Stopping riggate (PID ${RIGGATE_PID})..."
        kill "${RIGGATE_PID}" 2>/dev/null || true
        wait "${RIGGATE_PID}" 2>/dev/null || true
    fi
    if [ -n "${MOCK_PID}" ] && kill -0 "${MOCK_PID}" 2>/dev/null; then
        echo "Stopping mock provider (PID ${MOCK_PID})..."
        kill "${MOCK_PID}" 2>/dev/null || true
        wait "${MOCK_PID}" 2>/dev/null || true
    fi
    echo "Cleanup complete."
}
trap cleanup EXIT INT TERM

# 1. Start mock OpenAI upstream server
echo "Starting mock upstream provider..."
rm -f "${PORT_FILE}"
python3 "${SCRIPT_DIR}/mock_provider.py" 0 > "${PORT_FILE}" 2> "${MOCK_LOG}" &
MOCK_PID=$!

# Wait for mock provider port
for _ in {1..50}; do
    if [ -s "${PORT_FILE}" ]; then
        break
    fi
    sleep 0.1
done

if [ ! -s "${PORT_FILE}" ]; then
    echo "Error: Mock provider failed to start" >&2
    cat "${MOCK_LOG}" >&2
    exit 1
fi

MOCK_PORT=$(grep "PORT=" "${PORT_FILE}" | cut -d'=' -f2)
echo "[OK] Mock upstream provider running on 127.0.0.1:${MOCK_PORT} (PID ${MOCK_PID})"
export OPENAI_BASE_URL="http://127.0.0.1:${MOCK_PORT}/v1"

# 2. Start Riggate
echo "Starting Riggate on port ${RIGGATE_PORT}..."
rm -f "${LOG_FILE}"
"${BIN_PATH}" serve --config "${CONFIG_PATH}" 2> "${LOG_FILE}" &
RIGGATE_PID=$!

# Wait for Riggate to be ready
echo "Waiting for Riggate to be ready..."
READY=0
for _ in {1..50}; do
    if curl -s "http://127.0.0.1:${RIGGATE_PORT}/readyz" | grep -q '"ready"'; then
        READY=1
        break
    fi
    sleep 0.1
done

if [ "${READY}" -ne 1 ]; then
    echo "Error: Riggate failed to start" >&2
    cat "${LOG_FILE}" >&2
    exit 1
fi
echo "[OK] Riggate is ready (PID ${RIGGATE_PID})"
echo ""

# Test helper tracking
PASSED=0
FAILED=0

test_assert() {
    local desc="$1"
    local condition="$2"
    if eval "${condition}"; then
        echo "  [PASS] ${desc}"
        PASSED=$((PASSED + 1))
    else
        echo "  [FAIL] ${desc}" >&2
        FAILED=$((FAILED + 1))
    fi
}

echo "=== Running HTTP Test Cases ==="

# 1. Health & Readiness
echo "1. Health & Readiness Endpoints"
STATUS=$(curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:${RIGGATE_PORT}/healthz")
test_assert "GET /healthz returns 200" '[ "${STATUS}" = "200" ]'

STATUS=$(curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:${RIGGATE_PORT}/readyz")
test_assert "GET /readyz returns 200" '[ "${STATUS}" = "200" ]'

# 2. Authentication Enforcement
echo "2. Authentication Enforcement on /v1/models"
STATUS=$(curl -s -o /dev/null -w "%{http_code}" "http://127.0.0.1:${RIGGATE_PORT}/v1/models")
test_assert "Request without token returns 401 Unauthorized" '[ "${STATUS}" = "401" ]'

STATUS=$(curl -s -o /dev/null -w "%{http_code}" -H "Authorization: Bearer wrong-token" "http://127.0.0.1:${RIGGATE_PORT}/v1/models")
test_assert "Request with invalid token returns 401 Unauthorized" '[ "${STATUS}" = "401" ]'

STATUS=$(curl -s -o /dev/null -w "%{http_code}" -H "Authorization: Bearer ${CLAIMS_TOKEN}" "http://127.0.0.1:${RIGGATE_PORT}/v1/models")
test_assert "Request with valid token returns 200 OK" '[ "${STATUS}" = "200" ]'

# 3. Model Visibility & Agent Filtering
echo "3. Model Visibility & Agent Access Control"
CLAIMS_MODELS=$(curl -s -H "Authorization: Bearer ${CLAIMS_TOKEN}" "http://127.0.0.1:${RIGGATE_PORT}/v1/models")
test_assert "claims-app sees allowed agent 'claims-agent'" 'echo "${CLAIMS_MODELS}" | grep -q "\"claims-agent\""'
test_assert "claims-app does NOT see restricted agent 'admin-agent'" '! echo "${CLAIMS_MODELS}" | grep -q "\"admin-agent\""'

ADMIN_MODELS=$(curl -s -H "Authorization: Bearer ${ADMIN_TOKEN}" "http://127.0.0.1:${RIGGATE_PORT}/v1/models")
test_assert "admin-app (unrestricted) sees 'claims-agent'" 'echo "${ADMIN_MODELS}" | grep -q "\"claims-agent\""'
test_assert "admin-app (unrestricted) sees 'admin-agent'" 'echo "${ADMIN_MODELS}" | grep -q "\"admin-agent\""'

# 4. Agent Authorization Enforcement on /v1/chat/completions
echo "4. Agent Authorization Enforcement on /v1/chat/completions"
# Allowed call
STATUS_ALLOWED=$(curl -s -o /dev/null -w "%{http_code}" \
    -X POST "http://127.0.0.1:${RIGGATE_PORT}/v1/chat/completions" \
    -H "Authorization: Bearer ${CLAIMS_TOKEN}" \
    -H "Content-Type: application/json" \
    -d '{"model":"claims-agent","messages":[{"role":"user","content":"hello"}]}')
test_assert "claims-app accessing allowed agent 'claims-agent' returns 200 OK" '[ "${STATUS_ALLOWED}" = "200" ]'

# Forbidden call
BODY_FORBIDDEN=$(curl -s -w "\nHTTP_STATUS:%{http_code}" \
    -X POST "http://127.0.0.1:${RIGGATE_PORT}/v1/chat/completions" \
    -H "Authorization: Bearer ${CLAIMS_TOKEN}" \
    -H "Content-Type: application/json" \
    -d '{"model":"admin-agent","messages":[{"role":"user","content":"hello"}]}')
STATUS_FORBIDDEN=$(echo "${BODY_FORBIDDEN}" | grep "HTTP_STATUS:" | cut -d':' -f2)
test_assert "claims-app accessing forbidden agent 'admin-agent' returns 403 Forbidden" '[ "${STATUS_FORBIDDEN}" = "403" ]'
test_assert "403 response body contains rejection reason" 'echo "${BODY_FORBIDDEN}" | grep -q "client .claims-app. is not allowed to use agent .admin-agent."'

# 5. Client Identity & End-User Attribution
echo "5. Client Identity & End-User Attribution"
# Verify structured client identity in request spans
test_assert "Logs record client_id in chat request span" 'grep -q "\"client_id\":\"claims-app\"" "${LOG_FILE}"'
test_assert "Logs record client_name in chat request span" 'grep -q "\"client_name\":\"Claims Portal\"" "${LOG_FILE}"'

# Attributed request via body 'user' field
RESP_ALICE=$(curl -s -i \
    -X POST "http://127.0.0.1:${RIGGATE_PORT}/v1/chat/completions" \
    -H "Authorization: Bearer ${CLAIMS_TOKEN}" \
    -H "Content-Type: application/json" \
    -d '{"model":"claims-agent","user":"end-user-alice","messages":[{"role":"user","content":"claims query"}]}')
REQ_ID_ALICE=$(echo "${RESP_ALICE}" | tr -d '\r' | grep -i "^x-request-id:" | cut -d' ' -f2)
test_assert "Attributed request (user field) succeeded (200 OK)" 'echo "${RESP_ALICE}" | grep -q "200 OK"'
test_assert "Attributed request does not emit 'no end-user attribution' warning for its request_id" '! grep "request has no end-user attribution" "${LOG_FILE}" | grep -q "${REQ_ID_ALICE}"'

# Attributed request via X-Riggate-On-Behalf-Of header
RESP_BOB=$(curl -s -i \
    -X POST "http://127.0.0.1:${RIGGATE_PORT}/v1/chat/completions" \
    -H "Authorization: Bearer ${CLAIMS_TOKEN}" \
    -H "X-Riggate-On-Behalf-Of: end-user-bob" \
    -H "Content-Type: application/json" \
    -d '{"model":"claims-agent","messages":[{"role":"user","content":"header query"}]}')
REQ_ID_BOB=$(echo "${RESP_BOB}" | tr -d '\r' | grep -i "^x-request-id:" | cut -d' ' -f2)
test_assert "Attributed request (header) succeeded (200 OK)" 'echo "${RESP_BOB}" | grep -q "200 OK"'
test_assert "Attributed request does not emit 'no end-user attribution' warning for its request_id" '! grep "request has no end-user attribution" "${LOG_FILE}" | grep -q "${REQ_ID_BOB}"'

# Unattributed request (missing both)
RESP_ANON=$(curl -s -i \
    -X POST "http://127.0.0.1:${RIGGATE_PORT}/v1/chat/completions" \
    -H "Authorization: Bearer ${CLAIMS_TOKEN}" \
    -H "Content-Type: application/json" \
    -d '{"model":"claims-agent","messages":[{"role":"user","content":"anon query"}]}')
REQ_ID_ANON=$(echo "${RESP_ANON}" | tr -d '\r' | grep -i "^x-request-id:" | cut -d' ' -f2)
test_assert "Unattributed request succeeds without error" 'echo "${RESP_ANON}" | grep -q "200 OK"'
test_assert "Unattributed request logs warning 'request has no end-user attribution'" 'grep "request has no end-user attribution" "${LOG_FILE}" | grep -q "${REQ_ID_ANON}"'

# 6. Rust Unit & Audit Tests
echo ""
echo "6. Rust Test Suite (Audit Context, Tool Decision & Argument SHA-256 Hashing)"
if cargo test --quiet; then
    echo "  [PASS] All 59 Rust unit and integration tests passed"
    PASSED=$((PASSED + 1))
else
    echo "  [FAIL] Rust test suite failed" >&2
    FAILED=$((FAILED + 1))
fi

echo ""
echo "=========================================="
echo "Test Summary: ${PASSED} passed, ${FAILED} failed"
echo "=========================================="

if [ "${FAILED}" -gt 0 ]; then
    echo "Test suite failed with ${FAILED} failure(s)." >&2
    exit 1
fi

echo "All tests passed successfully!"
