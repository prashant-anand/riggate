#!/usr/bin/env bash
set -euo pipefail

# Riggate Local Test Setup Script
# Prepares the binary, prerequisites, and test environment

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
TEST_ENV_DIR="${REPO_ROOT}/target/test-env"

echo "=== Riggate Local Setup ==="
echo "Repository root: ${REPO_ROOT}"

# 1. Ensure Cargo is available
if ! command -v cargo &>/dev/null; then
    if [ -f "${HOME}/.cargo/bin/cargo" ]; then
        export PATH="${HOME}/.cargo/bin:${PATH}"
    else
        echo "Error: 'cargo' not found. Please install Rust." >&2
        exit 1
    fi
fi
echo "[OK] Found cargo: $(cargo --version)"

# 2. Check for python3 and curl
command -v python3 &>/dev/null || { echo "Error: python3 is required for local mock server" >&2; exit 1; }
echo "[OK] Found python3: $(python3 --version)"

command -v curl &>/dev/null || { echo "Error: curl is required for HTTP tests" >&2; exit 1; }
echo "[OK] Found curl: $(curl --version | head -n 1)"

# 3. Build riggate binary in debug mode
echo "Building riggate binary..."
cd "${REPO_ROOT}"
cargo build

# Verify build
BIN_PATH="${REPO_ROOT}/target/debug/riggate"
if [ ! -x "${BIN_PATH}" ]; then
    echo "Error: riggate binary not found at ${BIN_PATH}" >&2
    exit 1
fi
echo "[OK] riggate built: $(${BIN_PATH} --version)"

# 4. Prepare test environment directory
mkdir -p "${TEST_ENV_DIR}"
chmod +x "${SCRIPT_DIR}/"*.sh 2>/dev/null || true
chmod +x "${SCRIPT_DIR}/"*.py 2>/dev/null || true

# 5. Create a sample test configuration
cat > "${TEST_ENV_DIR}/riggate.test.toml" << 'EOF'
bind = "127.0.0.1:18080"
log_targets = ["stderr"]
max_request_bytes = 1048576
max_concurrent_requests = 16
request_timeout_seconds = 10
shutdown_grace_seconds = 2
strict_requests = false

[[agents]]
name = "claims-agent"
provider = "openai"
model = "mock-claims-model"
preamble = "You are a claims processing agent."

[[agents]]
name = "admin-agent"
provider = "openai"
model = "mock-admin-model"
preamble = "You are an admin management agent."

[[clients]]
id = "claims-app"
name = "Claims Portal"
token_env = "CLAIMS_TOKEN"
allowed_agents = ["claims-agent"]

[[clients]]
id = "admin-app"
name = "Admin Portal"
token_env = "ADMIN_TOKEN"
# No allowed_agents restriction: permits all agents
EOF

# Validate config with riggate check
echo "Validating test configuration..."
"${BIN_PATH}" check --config "${TEST_ENV_DIR}/riggate.test.toml"

echo ""
echo "=== Setup complete! ==="
echo "You can now run: ${SCRIPT_DIR}/test_local.sh"
