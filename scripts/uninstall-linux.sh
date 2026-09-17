#!/usr/bin/env bash
# Remove aihubd, launcher script, and supervision config from a Linux box; optional data purge.
set -euo pipefail

DRY_RUN=0
PURGE=0

LOCAL_BIN="${HOME}/.local/bin"
AIHUB_STATE_DIR="${HOME}/.local/share/aihub"
SERVICE_SCRIPT="${LOCAL_BIN}/aihubd-service"
PID_FILE="${AIHUB_STATE_DIR}/aihubd.pid"
SOCKET_FILE="${AIHUB_STATE_DIR}/aihub.sock"

usage() {
  cat <<'EOF'
Usage: uninstall-linux.sh [--dry-run] [--purge]

  --dry-run   Print planned actions without changing the system.
  --purge     Remove aihub data, logs, socket, and worktrees under ~/.local/share/aihub.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    --purge)
      PURGE=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "uninstall-linux.sh: unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

log() {
  printf 'uninstall-linux.sh: %s\n' "$*"
}

run() {
  if [[ "${DRY_RUN}" -eq 1 ]]; then
    log "[dry-run] $*"
  else
    log "+ $*"
    "$@"
  fi
}

stop_running_service() {
  if [[ -x "${SERVICE_SCRIPT}" ]]; then
    if [[ "${DRY_RUN}" -eq 1 ]]; then
      log "[dry-run] ${SERVICE_SCRIPT} stop"
    else
      log "Stopping running aihubd service..."
      "${SERVICE_SCRIPT}" stop || true
    fi
  elif [[ -f "${PID_FILE}" ]]; then
    local pid
    pid="$(tr -d '[:space:]' < "${PID_FILE}")"
    if [[ -n "${pid}" ]] && kill -0 "${pid}" 2>/dev/null; then
      if [[ "${DRY_RUN}" -eq 1 ]]; then
        log "[dry-run] kill -TERM ${pid}"
      else
        log "Killing running aihubd (PID ${pid})..."
        kill -TERM "${pid}" 2>/dev/null || true
      fi
    fi
  fi
}

remove_installed_files() {
  for bin in aihubd aihubd-service; do
    if [[ -e "${LOCAL_BIN}/${bin}" ]]; then
      run rm -f "${LOCAL_BIN}/${bin}"
    fi
  done

  # Also remove socket and pidfile unconditionally so no stale files remain
  run rm -f "${PID_FILE}" "${SOCKET_FILE}"
}

purge_aihub_data() {
  if [[ "${PURGE}" -ne 1 ]]; then
    log "Keeping data under ${AIHUB_STATE_DIR} (use --purge to delete)."
    return 0
  fi

  log "Purging aihub state directory (${AIHUB_STATE_DIR})..."
  run rm -rf "${AIHUB_STATE_DIR}"
}

main() {
  stop_running_service
  remove_installed_files
  purge_aihub_data
  log "done"
}

main "$@"
