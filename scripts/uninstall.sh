#!/usr/bin/env bash
# Remove aihub LaunchAgents and binaries; optional data purge.
set -euo pipefail

DRY_RUN=0
PURGE=0
PURGE_AI_MEMORY=0

LOCAL_BIN="${HOME}/.local/bin"
LAUNCH_AGENTS="${HOME}/Library/LaunchAgents"
INSTALL_AI_MEMORY_MARKER="${HOME}/.local/share/aihub/.installed-ai-memory-by-aihub"

AI_MEMORY_LABEL="com.github.akitaonrails.ai-memory"
AIHUBD_LABEL="io.mathborgess.aihubd"

usage() {
  cat <<'EOF'
Usage: uninstall.sh [--dry-run] [--purge] [--purge-ai-memory]

  --dry-run            Print planned actions without changing the system.
  --purge              Remove aihub data under ~/.local/share/aihub.
  --purge-ai-memory    Remove ai-memory data, config, and hooks paths (explicit opt-in).
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
    --purge-ai-memory)
      PURGE_AI_MEMORY=1
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
    log "[dry-run] launchctl bootout ${domain}/${AIHUBD_LABEL}"
    log "[dry-run] rm -f ${aihubd_plist}"
    if [[ -f "${INSTALL_AI_MEMORY_MARKER}" ]]; then
      log "[dry-run] launchctl bootout ${domain}/${AI_MEMORY_LABEL}"
      log "[dry-run] rm -f ${memory_plist}"
    else
      log "[dry-run] keeping ai-memory LaunchAgent (no install marker)"
    fi
    return 0
  fi

  launchctl bootout "${domain}/${AIHUBD_LABEL}" 2>/dev/null || true
  rm -f "${aihubd_plist}"

  if [[ -f "${INSTALL_AI_MEMORY_MARKER}" ]]; then
    launchctl bootout "${domain}/${AI_MEMORY_LABEL}" 2>/dev/null || true
    rm -f "${memory_plist}"
  else
    log "keeping ai-memory LaunchAgent (no install marker)"
  fi
}

remove_binaries() {
  local bin
  for bin in aihub aihubd; do
    if [[ -e "${LOCAL_BIN}/${bin}" ]]; then
      run rm -f "${LOCAL_BIN}/${bin}"
    fi
  done

  if [[ -f "${INSTALL_AI_MEMORY_MARKER}" ]]; then
    if [[ -e "${LOCAL_BIN}/ai-memory" ]]; then
      run rm -f "${LOCAL_BIN}/ai-memory"
    fi
    run rm -f "${INSTALL_AI_MEMORY_MARKER}"
  else
    log "keeping pre-existing ai-memory binary (no install marker)"
  fi
}

# --- ai-memory (transitional): delete this block when aihub-native memory ships ---
purge_ai_memory_data() {
  if [[ "${PURGE_AI_MEMORY}" -ne 1 ]]; then
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
  purge_aihub_data
  purge_ai_memory_data
  log "done"
}

main "$@"
