#!/usr/bin/env bash
# Launch aihubd with nohup and log rotation under Linux without systemd.
set -euo pipefail

AIHUB_STATE_DIR="${HOME}/.local/share/aihub"
LOG_DIR="${AIHUB_STATE_DIR}/log"
LOG_FILE="${LOG_DIR}/aihubd.log"
PID_FILE="${AIHUB_STATE_DIR}/aihubd.pid"
ENV_FILE="${AIHUB_STATE_DIR}/aihubd.env"
WORKTREES_DIR="${AIHUB_STATE_DIR}/worktrees"
SOCKET_FILE="${AIHUB_STATE_DIR}/aihub.sock"
AIHUBD_BIN="${HOME}/.local/bin/aihubd"
TCP_PORT=9920

# Max log file size before built-in rotation (default: 10MB)
MAX_LOG_BYTES="${AIHUBD_MAX_LOG_BYTES:-10485760}"
MAX_ROTATIONS="${AIHUBD_MAX_ROTATIONS:-5}"

usage() {
  cat <<'EOF'
Usage: aihubd-service.sh {start|stop|restart|status|rotate}

Commands:
  start    Start aihubd in background with nohup (idempotent, checks if already running).
  stop     Stop running aihubd process gracefully (SIGTERM -> SIGKILL).
  restart  Stop then start aihubd.
  status   Check running state, PID, socket, and log file size.
  rotate   Perform manual/on-demand rotation of aihubd.log.
EOF
}

log() {
  printf 'aihubd-service: %s\n' "$*"
}

warn() {
  printf 'aihubd-service: warning: %s\n' "$*" >&2
}

is_pid_running() {
  local pid="$1"
  if [[ -n "${pid}" ]] && kill -0 "${pid}" 2>/dev/null; then
    return 0
  fi
  return 1
}

get_running_pid() {
  if [[ -f "${PID_FILE}" ]]; then
    local pid
    pid="$(tr -d '[:space:]' < "${PID_FILE}")"
    if is_pid_running "${pid}"; then
      echo "${pid}"
      return 0
    fi
  fi
  # Secondary check: search via pgrep for aihubd binary matching user and path
  local detected
  detected="$(pgrep -u "$(id -u)" -f "${AIHUBD_BIN}" 2>/dev/null || true)"
  for pid in ${detected}; do
    if is_pid_running "${pid}"; then
      echo "${pid}"
      return 0
    fi
  done
  return 1
}

rotate_logs_builtin() {
  if [[ ! -f "${LOG_FILE}" ]]; then
    return 0
  fi

  local size=0
  if stat --version >/dev/null 2>&1; then
    # GNU coreutils stat
    size="$(stat -c %s "${LOG_FILE}" 2>/dev/null || echo 0)"
  else
    # BSD/macOS stat fallback
    size="$(stat -f %z "${LOG_FILE}" 2>/dev/null || echo 0)"
  fi

  if (( size < MAX_LOG_BYTES )); then
    return 0
  fi

  log "Rotating log ${LOG_FILE} (size ${size} bytes >= ${MAX_LOG_BYTES} bytes)..."
  local i
  for ((i = MAX_ROTATIONS - 1; i >= 1; i--)); do
    local next=$((i + 1))
    if [[ -f "${LOG_FILE}.${i}.gz" ]]; then
      mv -f "${LOG_FILE}.${i}.gz" "${LOG_FILE}.${next}.gz"
    elif [[ -f "${LOG_FILE}.${i}" ]]; then
      mv -f "${LOG_FILE}.${i}" "${LOG_FILE}.${next}"
    fi
  done

  # Rotate active log via copytruncate logic to preserve running writer
  local tmp_rotated="${LOG_FILE}.1"
  cp -p "${LOG_FILE}" "${tmp_rotated}"
  : > "${LOG_FILE}"

  if command -v gzip >/dev/null 2>&1; then
    gzip -f "${tmp_rotated}" 2>/dev/null || true
  fi
}

check_port_free() {
  local port="$1"
  if command -v ss >/dev/null 2>&1; then
    if ss -H -t -l -n "sport = :${port}" 2>/dev/null | grep -q "${port}"; then
      return 1
    fi
  elif command -v lsof >/dev/null 2>&1; then
    if lsof -iTCP:"${port}" -sTCP:LISTEN -P -n >/dev/null 2>&1; then
      return 1
    fi
  elif command -v netstat >/dev/null 2>&1; then
    if netstat -an 2>/dev/null | grep -E "(\.|\:)${port}\s+.*LISTEN" >/dev/null 2>&1; then
      return 1
    fi
  fi
  return 0
}

start_daemon() {
  mkdir -p "${AIHUB_STATE_DIR}" "${LOG_DIR}" "${WORKTREES_DIR}"

  local running_pid
  if running_pid="$(get_running_pid)"; then
    log "aihubd is already running (PID ${running_pid}). Not starting another instance."
    return 0
  fi

  # Clean stale pidfile if process dead
  rm -f "${PID_FILE}"

  # Clean stale socket file if left behind
  if [[ -e "${SOCKET_FILE}" ]]; then
    rm -f "${SOCKET_FILE}"
  fi

  if [[ ! -x "${AIHUBD_BIN}" ]]; then
    echo "aihubd-service: binary not found or not executable: ${AIHUBD_BIN}" >&2
    echo "aihubd-service: run scripts/install-linux.sh first." >&2
    exit 1
  fi

  if ! check_port_free "${TCP_PORT}"; then
    echo "aihubd-service: port ${TCP_PORT} is already in use by another process!" >&2
    echo "aihubd-service: check conflicting services (reports, hermes, broker, stub) before starting." >&2
    exit 1
  fi

  # Check and rotate logs if needed before launching
  rotate_logs_builtin

  # Source environment file if present (PATH, custom TMPDIR, etc.)
  if [[ -f "${ENV_FILE}" ]]; then
    set -a
    # shellcheck source=/dev/null
    source "${ENV_FILE}"
    set +a
  fi

  # Enforce worktrees do not default to volatile /tmp tmpfs
  export TMPDIR="${TMPDIR:-${AIHUB_STATE_DIR}}"

  log "Launching aihubd with nohup..."
  touch "${LOG_FILE}"
  chmod 0600 "${LOG_FILE}"

  nohup "${AIHUBD_BIN}" --socket "${SOCKET_FILE}" >> "${LOG_FILE}" 2>&1 &
  local new_pid=$!
  echo "${new_pid}" > "${PID_FILE}"
  log "aihubd started (PID ${new_pid}). Logs at ${LOG_FILE}"

  # Verification loop (up to 5s)
  local deadline=$((SECONDS + 5))
  local ready=0
  while (( SECONDS < deadline )); do
    if ! is_pid_running "${new_pid}"; then
      echo "aihubd-service: aihubd process ${new_pid} died immediately after launch!" >&2
      tail -n 20 "${LOG_FILE}" >&2
      rm -f "${PID_FILE}"
      return 1
    fi
    if [[ -S "${SOCKET_FILE}" ]]; then
      ready=1
      break
    fi
    sleep 0.2
  done

  if [[ "${ready}" -eq 1 ]]; then
    log "aihubd socket verified: ${SOCKET_FILE}"
  else
    warn "aihubd started (PID ${new_pid}) but socket not yet visible after 5s. Check ${LOG_FILE}"
  fi
}

stop_daemon() {
  local running_pid
  if ! running_pid="$(get_running_pid)"; then
    log "aihubd is not running."
    rm -f "${PID_FILE}"
    return 0
  fi

  log "Stopping aihubd (PID ${running_pid})..."
  kill -TERM "${running_pid}" 2>/dev/null || true

  local deadline=$((SECONDS + 10))
  while (( SECONDS < deadline )); do
    if ! is_pid_running "${running_pid}"; then
      rm -f "${PID_FILE}" "${SOCKET_FILE}"
      log "aihubd stopped gracefully."
      return 0
    fi
    sleep 0.3
  done

  warn "aihubd (PID ${running_pid}) did not stop within 10s; sending SIGKILL..."
  kill -KILL "${running_pid}" 2>/dev/null || true
  rm -f "${PID_FILE}" "${SOCKET_FILE}"
  log "aihubd killed."
}

status_daemon() {
  local running_pid
  if running_pid="$(get_running_pid)"; then
    log "aihubd status: RUNNING (PID ${running_pid})"
    if [[ -S "${SOCKET_FILE}" ]]; then
      log "  socket: ${SOCKET_FILE} (active)"
    else
      log "  socket: ${SOCKET_FILE} (absent / not ready)"
    fi
  else
    log "aihubd status: STOPPED"
  fi

  if [[ -f "${LOG_FILE}" ]]; then
    local size=0
    if stat --version >/dev/null 2>&1; then
      size="$(stat -c %s "${LOG_FILE}" 2>/dev/null || echo 0)"
    else
      size="$(stat -f %z "${LOG_FILE}" 2>/dev/null || echo 0)"
    fi
    log "  log file: ${LOG_FILE} (${size} bytes)"
  else
    log "  log file: ${LOG_FILE} (not created yet)"
  fi
  log "  worktrees: ${WORKTREES_DIR}"
}

cmd="${1:-}"
case "${cmd}" in
  start)
    start_daemon
    ;;
  stop)
    stop_daemon
    ;;
  restart)
    stop_daemon
    start_daemon
    ;;
  status)
    status_daemon
    ;;
  rotate)
    rotate_logs_builtin
    ;;
  -h|--help|help)
    usage
    exit 0
    ;;
  *)
    usage >&2
    exit 2
    ;;
esac
