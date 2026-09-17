#!/usr/bin/env bash
# Isolated regression tests for install-linux.sh and aihubd-service.sh. Never touches real $HOME.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT

fail() {
  echo "test-install-linux.sh: $*" >&2
  exit 1
}

declare -a CLEANUP_DIRS=()
cleanup() {
  local dir
  for dir in "${CLEANUP_DIRS[@]:-}"; do
    [[ -n "${dir}" ]] && rm -rf "${dir}"
  done
}
trap cleanup EXIT

assert_eq() {
  local got="$1"
  local want="$2"
  local msg="$3"
  if [[ "${got}" != "${want}" ]]; then
    fail "${msg}: got '${got}', want '${want}'"
  fi
}

assert_path_exists() {
  local path="$1"
  local msg="$2"
  if [[ ! -e "${path}" ]]; then
    fail "${msg}: missing ${path}"
  fi
}

assert_path_missing() {
  local path="$1"
  local msg="$2"
  if [[ -e "${path}" ]]; then
    fail "${msg}: unexpected ${path}"
  fi
}

make_fake_aihubd() {
  local dir="$1"
  mkdir -p "${dir}"
  # Fake aihubd that binds a unix domain socket and listens until SIGTERM
  cat >"${dir}/aihubd" <<'EOF'
#!/usr/bin/env bash
socket_path=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --socket)
      socket_path="$2"
      shift 2
      ;;
    *)
      shift
      ;;
  esac
done

if [[ -z "${socket_path}" ]]; then
  echo "fake-aihubd: missing --socket" >&2
  exit 1
fi

mkdir -p "$(dirname "${socket_path}")"
# Create unix domain socket using python if available, or nc -l -U, or fallback touch/fifo
python3 -c "import socket, sys, os, signal; s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.bind(sys.argv[1]); s.listen(1); signal.pause()" "${socket_path}" &
py_pid=$!

term_handler() {
  kill -TERM "${py_pid}" 2>/dev/null || true
  rm -f "${socket_path}"
  exit 0
}
trap term_handler TERM INT EXIT
wait "${py_pid}"
EOF
  chmod +x "${dir}/aihubd"

  # Optional fake aihub client
  cat >"${dir}/aihub" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
  chmod +x "${dir}/aihub"
}

setup_isolated_home() {
  local parent
  parent="$(mktemp -d "/tmp/ahlt.XXXXXX")"
  CLEANUP_DIRS+=("${parent}")
  ISOLATED_HOME="${parent}/u"
  mkdir -p "${ISOLATED_HOME}"

  STUB_BIN="${ISOLATED_HOME}/stub-bin"
  mkdir -p "${STUB_BIN}"

  # Fake cc command to satisfy prerequisite
  cat >"${STUB_BIN}/cc" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
  chmod +x "${STUB_BIN}/cc"

  FAKE_BIN_DIR="${ISOLATED_HOME}/prebuilt"
  make_fake_aihubd "${FAKE_BIN_DIR}"
}

test_fresh_install_and_layout() {
  setup_isolated_home
  HOME="${ISOLATED_HOME}" PATH="${STUB_BIN}:${PATH}" \
    "${ROOT}/scripts/install-linux.sh" --no-start --bin-dir "${FAKE_BIN_DIR}"

  assert_path_exists "${ISOLATED_HOME}/.local/bin/aihubd" "aihubd binary installed"
  assert_path_exists "${ISOLATED_HOME}/.local/bin/aihubd-service" "launcher service installed"
  assert_path_exists "${ISOLATED_HOME}/.local/share/aihub" "aihub state directory created"
  assert_path_exists "${ISOLATED_HOME}/.local/share/aihub/log" "log directory created"
  assert_path_exists "${ISOLATED_HOME}/.local/share/aihub/worktrees" "worktrees directory created"
  assert_path_exists "${ISOLATED_HOME}/.local/share/aihub/aihubd.env" "environment file written"
  assert_path_exists "${ISOLATED_HOME}/.local/share/aihub/logrotate.conf" "logrotate config written"

  # Check permissions on sensitive directories
  local mode
  if stat --version >/dev/null 2>&1; then
    mode="$(stat -c %a "${ISOLATED_HOME}/.local/share/aihub")"
  else
    mode="$(stat -f %Lp "${ISOLATED_HOME}/.local/share/aihub")"
  fi
  assert_eq "${mode}" "700" "aihub state dir permissions"
}

test_service_start_stop_idempotent() {
  setup_isolated_home
  HOME="${ISOLATED_HOME}" PATH="${STUB_BIN}:${PATH}" \
    "${ROOT}/scripts/install-linux.sh" --no-start --bin-dir "${FAKE_BIN_DIR}"

  local service_script="${ISOLATED_HOME}/.local/bin/aihubd-service"

  # 1. Start service
  HOME="${ISOLATED_HOME}" PATH="${STUB_BIN}:${PATH}" "${service_script}" start

  assert_path_exists "${ISOLATED_HOME}/.local/share/aihub/aihubd.pid" "pidfile created on start"
  assert_path_exists "${ISOLATED_HOME}/.local/share/aihub/aihub.sock" "socket created on start"

  local pid1
  pid1="$(cat "${ISOLATED_HOME}/.local/share/aihub/aihubd.pid")"

  # 2. Second start does not duplicate process
  HOME="${ISOLATED_HOME}" PATH="${STUB_BIN}:${PATH}" "${service_script}" start
  local pid2
  pid2="$(cat "${ISOLATED_HOME}/.local/share/aihub/aihubd.pid")"
  assert_eq "${pid1}" "${pid2}" "idempotent start does not duplicate pid"

  # 3. Status command
  HOME="${ISOLATED_HOME}" PATH="${STUB_BIN}:${PATH}" "${service_script}" status

  # 4. Stop service
  HOME="${ISOLATED_HOME}" PATH="${STUB_BIN}:${PATH}" "${service_script}" stop
  assert_path_missing "${ISOLATED_HOME}/.local/share/aihub/aihub.sock" "socket cleaned up on stop"

  # 5. Stop when already stopped is idempotent
  HOME="${ISOLATED_HOME}" PATH="${STUB_BIN}:${PATH}" "${service_script}" stop
}

test_log_rotation() {
  setup_isolated_home
  HOME="${ISOLATED_HOME}" PATH="${STUB_BIN}:${PATH}" \
    "${ROOT}/scripts/install-linux.sh" --no-start --bin-dir "${FAKE_BIN_DIR}"

  local service_script="${ISOLATED_HOME}/.local/bin/aihubd-service"
  local log_file="${ISOLATED_HOME}/.local/share/aihub/log/aihubd.log"

  # Create oversized log file (> 1000 bytes with low limit)
  mkdir -p "$(dirname "${log_file}")"
  head -c 2000 </dev/urandom > "${log_file}"

  # Run rotate with 1000 byte limit
  HOME="${ISOLATED_HOME}" PATH="${STUB_BIN}:${PATH}" AIHUBD_MAX_LOG_BYTES=1000 \
    "${service_script}" rotate

  # Should have created .1 or .1.gz and truncated active log
  if [[ -f "${log_file}.1" ]] || [[ -f "${log_file}.1.gz" ]]; then
    : # Success
  else
    fail "log rotation did not create rotated file"
  fi

  local active_size
  if stat --version >/dev/null 2>&1; then
    active_size="$(stat -c %s "${log_file}")"
  else
    active_size="$(stat -f %z "${log_file}")"
  fi
  assert_eq "${active_size}" "0" "active log truncated"
}

test_uninstall_and_purge() {
  setup_isolated_home
  HOME="${ISOLATED_HOME}" PATH="${STUB_BIN}:${PATH}" \
    "${ROOT}/scripts/install-linux.sh" --no-start --bin-dir "${FAKE_BIN_DIR}"

  # Touch custom data file inside state dir
  touch "${ISOLATED_HOME}/.local/share/aihub/custom.txt"

  # Run uninstall without purge
  HOME="${ISOLATED_HOME}" PATH="${STUB_BIN}:${PATH}" \
    "${ROOT}/scripts/uninstall-linux.sh"

  assert_path_missing "${ISOLATED_HOME}/.local/bin/aihubd" "aihubd binary removed"
  assert_path_missing "${ISOLATED_HOME}/.local/bin/aihubd-service" "aihubd-service script removed"
  assert_path_exists "${ISOLATED_HOME}/.local/share/aihub/custom.txt" "state kept without --purge"

  # Run uninstall with purge
  HOME="${ISOLATED_HOME}" PATH="${STUB_BIN}:${PATH}" \
    "${ROOT}/scripts/uninstall-linux.sh" --purge

  assert_path_missing "${ISOLATED_HOME}/.local/share/aihub" "state directory purged"
}

main() {
  test_fresh_install_and_layout
  test_service_start_stop_idempotent
  test_log_rotation
  test_uninstall_and_purge
  echo "test-install-linux.sh: all cases passed"
}

main "$@"
