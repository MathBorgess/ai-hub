#!/usr/bin/env bash
# Remove aihub LaunchAgents and binaries; optional data purge.
set -euo pipefail

DRY_RUN=0
PURGE=0

LOCAL_BIN="${HOME}/.local/bin"
LAUNCH_AGENTS="${HOME}/Library/LaunchAgents"

AI_MEMORY_LABEL="com.github.akitaonrails.ai-memory"
AIHUBD_LABEL="io.mathborgess.aihubd"

usage() {
  cat <<'EOF'
Usage: uninstall.sh [--dry-run] [--purge]

  --dry-run   Print planned actions without changing the system.
  --purge     Remove ai-memory and aihub data directories and config.
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
      echo "uninstall.sh: unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

log() {
  printf 'uninstall.sh: %s\n' "$*"
}

run() {
  if [[ "${DRY_RUN}" -eq 1 ]]; then
    log "[dry-run] $*"
  else
    log "+ $*"
    "$@"
  fi
}

unregister_launchd() {
  local uid domain
  uid="$(id -u)"
  domain="gui/${uid}"

  local memory_plist="${LAUNCH_AGENTS}/com.github.akitaonrails.ai-memory.plist"
  local aihubd_plist="${LAUNCH_AGENTS}/io.mathborgess.aihubd.plist"

  if [[ "${DRY_RUN}" -eq 1 ]]; then
    log "[dry-run] launchctl bootout ${domain}/${AI_MEMORY_LABEL}"
    log "[dry-run] launchctl bootout ${domain}/${AIHUBD_LABEL}"
    log "[dry-run] rm -f ${memory_plist} ${aihubd_plist}"
    return 0
  fi

  launchctl bootout "${domain}/${AI_MEMORY_LABEL}" 2>/dev/null || true
  launchctl bootout "${domain}/${AIHUBD_LABEL}" 2>/dev/null || true
  rm -f "${memory_plist}" "${aihubd_plist}"
}

remove_binaries() {
  local bin
  for bin in aihub aihubd ai-memory; do
    if [[ -e "${LOCAL_BIN}/${bin}" ]]; then
      run rm -f "${LOCAL_BIN}/${bin}"
    fi
  done
}

# --- ai-memory (transitional): delete this block when aihub-native memory ships ---
purge_ai_memory_data() {
  if [[ "${PURGE}" -ne 1 ]]; then
    return 0
  fi
  run rm -rf "${HOME}/.local/share/ai-memory"
  run rm -rf "${HOME}/.config/ai-memory"
  run rm -rf "${HOME}/Library/Application Support/ai-memory"
  run rm -rf "${HOME}/.local/share/ai-memory-hooks"
}
# --- end ai-memory (transitional) ---

purge_aihub_data() {
  if [[ "${PURGE}" -ne 1 ]]; then
    return 0
  fi
  run rm -rf "${HOME}/.local/share/aihub"
}

main() {
  unregister_launchd
  remove_binaries
  purge_ai_memory_data
  purge_aihub_data
  log "done"
}

main "$@"
