#!/usr/bin/env bash
# End-to-end proof of the ai-memory handoff loop, entirely in temp dirs on
# loopback: download the pinned release (reusing install.sh's fetch/verify),
# init a data dir, serve on a free non-default port, then run
# session10_ai_memory_live_record_spool_and_drain against it.
#
# Never touches the real $HOME, launchd, or the default ai-memory port 49374.
set -euo pipefail

SELF_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Reuse install.sh's asset naming, checksum pin, and download/verify function.
# Sourcing (not executing) skips install.sh's main(), see its trailing guard.
# install.sh declares its own readonly ROOT from its own BASH_SOURCE.
# shellcheck source=scripts/install.sh
source "${SELF_DIR}/install.sh"

TMPDIR_E2E="$(mktemp -d)"
SERVER_PID=""

cleanup() {
  if [[ -n "${SERVER_PID}" ]] && kill -0 "${SERVER_PID}" 2>/dev/null; then
    kill "${SERVER_PID}" 2>/dev/null || true
    wait "${SERVER_PID}" 2>/dev/null || true
  fi
  rm -rf "${TMPDIR_E2E}"
}
trap cleanup EXIT

free_loopback_port() {
  python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
PY
}

log "fetching and verifying pinned ai-memory ${AI_MEMORY_VERSION} into ${TMPDIR_E2E}/bin"
mkdir -p "${TMPDIR_E2E}/bin"
ai_memory_fetch_verified "${TMPDIR_E2E}/bin"
AI_MEMORY_BIN="${TMPDIR_E2E}/bin/ai-memory"
chmod +x "${AI_MEMORY_BIN}"
export AI_MEMORY_BIN

AI_MEMORY_DATA_DIR="${TMPDIR_E2E}/ai-memory-data"
AI_MEMORY_CONFIG="${TMPDIR_E2E}/ai-memory-config.toml"
AIHUB_DATA_DIR="${TMPDIR_E2E}/aihub-data"
mkdir -p "${AI_MEMORY_DATA_DIR}" "${AIHUB_DATA_DIR}"
export AI_MEMORY_DATA_DIR AIHUB_DATA_DIR

log "init ai-memory data dir ${AI_MEMORY_DATA_DIR}"
"${AI_MEMORY_BIN}" --data-dir "${AI_MEMORY_DATA_DIR}" --config "${AI_MEMORY_CONFIG}" init

AI_MEMORY_PORT="$(free_loopback_port)"
export AI_MEMORY_PORT
# The config-file `bind` key wins over --bind in this ai-memory build; set it
# via its env override instead (documented in config.toml: AI_MEMORY_BIND).
export AI_MEMORY_BIND="127.0.0.1:${AI_MEMORY_PORT}"
# The `handoffs` CLI (invoked by the test as a subprocess) resolves the
# server to talk to from this env var, not from --data-dir.
export AI_MEMORY_SERVER_URL="http://127.0.0.1:${AI_MEMORY_PORT}"
log "starting ai-memory serve on 127.0.0.1:${AI_MEMORY_PORT} (non-default port, loopback only)"
"${AI_MEMORY_BIN}" serve \
  --transport http \
  --data-dir "${AI_MEMORY_DATA_DIR}" \
  --config "${AI_MEMORY_CONFIG}" \
  >"${TMPDIR_E2E}/ai-memory.stdout.log" 2>"${TMPDIR_E2E}/ai-memory.stderr.log" &
SERVER_PID=$!

for _ in $(seq 1 50); do
  if curl -sS -o /dev/null "http://127.0.0.1:${AI_MEMORY_PORT}/mcp"; then
    break
  fi
  sleep 0.1
done

export AI_MEMORY_E2E=1
log "running session10_ai_memory_live_record_spool_and_drain"
(
  cd "${ROOT}"
  export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"
  export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
  export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0
  cargo test -p aihub-memory --offline --test session10_ai_memory_loop -- --nocapture
)

log "done"
