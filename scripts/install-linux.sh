#!/usr/bin/env bash
# Install aihubd and supervision scripts on a Linux box without systemd.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT

DRY_RUN=0
NO_START=0
BIN_DIR=""

LOCAL_BIN="${HOME}/.local/bin"
AIHUB_STATE_DIR="${HOME}/.local/share/aihub"
LOG_DIR="${AIHUB_STATE_DIR}/log"
WORKTREES_DIR="${AIHUB_STATE_DIR}/worktrees"
SOCKET_FILE="${AIHUB_STATE_DIR}/aihub.sock"
ENV_FILE="${AIHUB_STATE_DIR}/aihubd.env"
SERVICE_SCRIPT="${LOCAL_BIN}/aihubd-service"
TCP_PORT=9920

usage() {
  cat <<'EOF'
Usage: install-linux.sh [--dry-run] [--no-start] [--bin-dir DIR]

  --dry-run      Print planned actions without changing the system.
  --no-start     Install binaries and scripts but do not launch aihubd.
  --bin-dir DIR  Install prebuilt aihub and aihubd from DIR instead of cargo build/install.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    --no-start)
      NO_START=1
      shift
      ;;
    --bin-dir)
      if [[ $# -lt 2 ]]; then
        echo "install-linux.sh: --bin-dir requires a directory argument" >&2
        exit 2
      fi
      BIN_DIR="$2"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "install-linux.sh: unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

log() {
  printf 'install-linux.sh: %s\n' "$*"
}

warn() {
  printf 'install-linux.sh: warning: %s\n' "$*" >&2
}

run() {
  if [[ "${DRY_RUN}" -eq 1 ]]; then
    log "[dry-run] $*"
  else
    log "+ $*"
    "$@"
  fi
}

need_cmd() {
  local name="$1"
  local fix="$2"
  if ! command -v "${name}" >/dev/null 2>&1; then
    echo "install-linux.sh: missing required command: ${name}" >&2
    echo "install-linux.sh: ${fix}" >&2
    return 1
  fi
}

version_ge() {
  local IFS=.
  local -a a b
  read -r -a a <<<"$1"
  read -r -a b <<<"$2"
  local i max="${#a[@]}"
  if [[ "${#b[@]}" -gt "${max}" ]]; then
    max="${#b[@]}"
  fi
  for ((i = 0; i < max; i++)); do
    local av="${a[i]:-0}"
    local bv="${b[i]:-0}"
    if ((10#${av} > 10#${bv})); then
      return 0
    fi
    if ((10#${av} < 10#${bv})); then
      return 1
    fi
  done
  return 0
}

read_rust_version_ms() {
  local line
  line="$(grep -E '^rust-version\s*=' "${ROOT}/Cargo.toml" | head -n1 || true)"
  line="${line#*\"}"
  line="${line%\"*}"
  echo "${line}"
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

check_prerequisites() {
  local missing=0

  # Auto-source ~/.cargo/env if present and rustc not yet on PATH
  if ! command -v rustc >/dev/null 2>&1 && [[ -f "${HOME}/.cargo/env" ]]; then
    # shellcheck source=/dev/null
    source "${HOME}/.cargo/env"
  fi

  # 1. C compiler for rusqlite bundled
  if ! command -v cc >/dev/null 2>&1 && ! command -v gcc >/dev/null 2>&1; then
    echo "install-linux.sh: missing C compiler (cc/gcc). build-essential is required for rusqlite bundled." >&2
    echo "install-linux.sh: on Debian/Ubuntu, run: apt-get install -y build-essential" >&2
    missing=1
  fi

  # 2. Port check (9920 loopback)
  if ! check_port_free "${TCP_PORT}"; then
    echo "install-linux.sh: port ${TCP_PORT} is already occupied!" >&2
    echo "install-linux.sh: ensure no conflicting stub or process is listening on ${TCP_PORT} (8787=reports, 9900=hermes, 9910=broker)." >&2
    missing=1
  fi

  # 3. Binaries or toolchain check
  if [[ -n "${BIN_DIR}" ]]; then
    if [[ ! -d "${BIN_DIR}" ]]; then
      echo "install-linux.sh: --bin-dir is not a directory: ${BIN_DIR}" >&2
      missing=1
    elif [[ ! -x "${BIN_DIR}/aihubd" ]]; then
      echo "install-linux.sh: --bin-dir must contain executable aihubd" >&2
      missing=1
    fi
  else
    local min_rust
    min_rust="$(read_rust_version_ms)"
    if ! need_cmd git 'Install git via package manager (e.g. apt-get install -y git)'; then
      missing=1
    fi
    if ! need_cmd cargo 'Rust toolchain 1.98.1 required. Source ~/.cargo/env or run: rustup toolchain install 1.98.1'; then
      missing=1
    else
      local rustc_ver
      rustc_ver="$(rustc --version | awk '{print $2}')"
      if ! version_ge "${rustc_ver}" "${min_rust}"; then
        echo "install-linux.sh: rustc ${rustc_ver} is older than workspace rust-version ${min_rust}" >&2
        echo "install-linux.sh: activate rustup 1.98.1: source ~/.cargo/env && rustup default 1.98.1" >&2
        missing=1
      fi
    fi
  fi

  if [[ "${missing}" -ne 0 ]]; then
    exit 1
  fi
}

path_unique_dirs() {
  local -a inputs=("$@")
  local -a out=()
  local dir seen d
  for dir in "${inputs[@]}"; do
    seen=0
    for d in "${out[@]:-}"; do
      if [[ "${d}" == "${dir}" ]]; then
        seen=1
        break
      fi
    done
    if [[ "${seen}" -eq 0 ]]; then
      out+=("${dir}")
    fi
  done
  local IFS=:
  echo "${out[*]}"
}

harness_runtime_dirs_for_script() {
  local script_path="$1"
  local line interpreter env_bin prog interp_path
  local -a parts=()

  if [[ ! -f "${script_path}" ]]; then
    return 0
  fi
  IFS= read -r line <"${script_path}" || return 0
  if [[ "${line}" != '#!'* ]]; then
    return 0
  fi
  interpreter="${line#\#!}"
  interpreter="${interpreter#"${interpreter%%[![:space:]]*}"}"
  read -r -a parts <<<"${interpreter}"
  if [[ "${#parts[@]}" -eq 0 ]]; then
    return 0
  fi
  env_bin="${parts[0]}"
  if [[ "$(basename "${env_bin}")" == "env" ]]; then
    prog="${parts[1]:-}"
    if [[ -z "${prog}" ]]; then
      return 0
    fi
    if interp_path="$(command -v "${prog}" 2>/dev/null)"; then
      printf '%s\n' "$(dirname "${interp_path}")"
    else
      warn "harness interpreter not found in installing shell PATH: ${prog} (${script_path})"
    fi
  elif [[ "${env_bin}" == /* ]]; then
    printf '%s\n' "$(dirname "${env_bin}")"
  fi
}

resolve_harness_path() {
  local -a harness_names=(claude codex cursor-agent agy)
  local -a harness_dirs=("${LOCAL_BIN}")
  local name path runtime_dir
  for name in "${harness_names[@]}"; do
    if path="$(command -v "${name}" 2>/dev/null)"; then
      harness_dirs+=("$(dirname "${path}")")
      while IFS= read -r runtime_dir; do
        [[ -n "${runtime_dir}" ]] && harness_dirs+=("${runtime_dir}")
      done < <(harness_runtime_dirs_for_script "${path}")
    else
      warn "harness not found in installing shell PATH: ${name}"
    fi
  done

  local extra="/usr/local/bin:/usr/bin:/bin:/usr/local/sbin:/usr/sbin:/sbin"
  local unique
  unique="$(path_unique_dirs "${harness_dirs[@]}")"
  echo "${unique}:${extra}"
}

create_layout() {
  run mkdir -p "${LOCAL_BIN}" "${AIHUB_STATE_DIR}" "${LOG_DIR}" "${WORKTREES_DIR}"
  run chmod 0700 "${AIHUB_STATE_DIR}" "${LOG_DIR}" "${WORKTREES_DIR}"
}

install_binaries() {
  if [[ -n "${BIN_DIR}" ]]; then
    run install -m 0755 "${BIN_DIR}/aihubd" "${LOCAL_BIN}/aihubd"
    if [[ -x "${BIN_DIR}/aihub" ]]; then
      run install -m 0755 "${BIN_DIR}/aihub" "${LOCAL_BIN}/aihub"
    fi
  else
    # Build aihubd in release mode on the box
    log "Building aihubd with cargo build --release -p aihubd..."
    run cargo build --release --locked --manifest-path "${ROOT}/Cargo.toml" -p aihubd
    run install -m 0755 "${ROOT}/target/release/aihubd" "${LOCAL_BIN}/aihubd"
    # If aihub client was built or requested, install it optionally
    if [[ -f "${ROOT}/target/release/aihub" ]]; then
      run install -m 0755 "${ROOT}/target/release/aihub" "${LOCAL_BIN}/aihub"
    fi
  fi
}

install_supervision_files() {
  # Install the launcher service script
  run install -m 0755 "${ROOT}/packaging/bin/aihubd-service.sh" "${SERVICE_SCRIPT}"

  # Write the aihubd.env file atomically
  local runtime_path
  runtime_path="$(resolve_harness_path)"

  if [[ "${DRY_RUN}" -eq 1 ]]; then
    log "[dry-run] generate ${ENV_FILE} with PATH=${runtime_path}"
    log "[dry-run] copy packaging/logrotate/aihubd.conf to ${AIHUB_STATE_DIR}/logrotate.conf"
  else
    local tmp_env
    tmp_env="$(mktemp "${ENV_FILE}.XXXXXX")"
    cat >"${tmp_env}" <<EOF
# Generated by install-linux.sh on $(date -u +"%Y-%m-%dT%H:%M:%SZ")
PATH="${runtime_path}"
TMPDIR="${WORKTREES_DIR}"
AIHUBD_PORT="${TCP_PORT}"
EOF
    chmod 0600 "${tmp_env}"
    mv "${tmp_env}" "${ENV_FILE}"

    # Install logrotate configuration for optional logrotate invocation
    if [[ -f "${ROOT}/packaging/logrotate/aihubd.conf" ]]; then
      cp "${ROOT}/packaging/logrotate/aihubd.conf" "${AIHUB_STATE_DIR}/logrotate.conf"
      chmod 0644 "${AIHUB_STATE_DIR}/logrotate.conf"
    fi
  fi
}

restart_service() {
  if [[ "${NO_START}" -eq 1 ]] || [[ "${DRY_RUN}" -eq 1 ]]; then
    if [[ "${NO_START}" -eq 1 ]]; then
      log "skipping service start (--no-start)"
    fi
    return 0
  fi

  log "Starting/restarting aihubd via ${SERVICE_SCRIPT}..."
  "${SERVICE_SCRIPT}" restart
}

check_readiness() {
  if [[ "${NO_START}" -eq 1 ]] || [[ "${DRY_RUN}" -eq 1 ]]; then
    return 0
  fi

  local deadline=$((SECONDS + 15))
  local ready=0
  while (( SECONDS < deadline )); do
    if [[ -S "${SOCKET_FILE}" ]]; then
      ready=1
      break
    fi
    sleep 0.5
  done

  if [[ "${ready}" -eq 1 ]]; then
    log "aihubd is ready and listening on socket: ${SOCKET_FILE}"
  else
    warn "aihubd socket not ready after 15s. Check logs with: tail -n 30 ${LOG_DIR}/aihubd.log"
  fi
}

main() {
  log "Installing aihubd for Linux (non-systemd box)..."
  check_prerequisites
  create_layout
  install_binaries
  install_supervision_files
  restart_service
  check_readiness
  log "Installation complete!"
  log "Commands available:"
  log "  Service control: ${SERVICE_SCRIPT} {start|stop|restart|status|rotate}"
  log "  Log file:        ${LOG_DIR}/aihubd.log"
  log "  Worktrees root:  ${WORKTREES_DIR}"
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
  main "$@"
fi
