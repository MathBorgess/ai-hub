#!/usr/bin/env bash
# Install aihub, aihubd, ai-memory (transitional), and macOS LaunchAgents.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT

DRY_RUN=0
WITH_AGENT_HOOKS=0
NO_START=0
BIN_DIR=""

LOCAL_BIN="${HOME}/.local/bin"
LOCAL_ROOT="${HOME}/.local"
AIHUB_SOCKET="${HOME}/.local/share/aihub/aihub.sock"
AIHUB_STATE_DIR="${HOME}/.local/share/aihub"
INSTALL_AI_MEMORY_MARKER="${AIHUB_STATE_DIR}/.installed-ai-memory-by-aihub"
LOG_DIR="${HOME}/Library/Logs/aihub"
LAUNCH_AGENTS="${HOME}/Library/LaunchAgents"

AI_MEMORY_LABEL="com.github.akitaonrails.ai-memory"
AIHUBD_LABEL="io.mathborgess.aihubd"

READINESS_DEADLINE_S=30

usage() {
  cat <<'EOF'
Usage: install.sh [--dry-run] [--with-agent-hooks] [--no-start] [--bin-dir DIR]

  --dry-run            Print planned actions without changing the system.
  --with-agent-hooks   Run ai-memory install-mcp/install-hooks for supported agents.
  --no-start           Install files and plists but skip launchctl bootstrap and readiness checks.
  --bin-dir DIR        Install prebuilt aihub and aihubd from DIR instead of cargo install.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run)
      DRY_RUN=1
      shift
      ;;
    --with-agent-hooks)
      WITH_AGENT_HOOKS=1
      shift
      ;;
    --no-start)
      NO_START=1
      shift
      ;;
    --bin-dir)
      if [[ $# -lt 2 ]]; then
        echo "install.sh: --bin-dir requires a directory argument" >&2
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
      echo "install.sh: unknown option: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

log() {
  printf 'install.sh: %s\n' "$*"
}

warn() {
  printf 'install.sh: warning: %s\n' "$*" >&2
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
    echo "install.sh: missing required command: ${name}" >&2
    echo "install.sh: ${fix}" >&2
    return 1
  fi
}

version_ge() {
  # True when $1 >= $2 (semver numeric segments).
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

check_prerequisites() {
  local missing=0

  if ! need_cmd plutil 'plutil ships with macOS.'; then
    missing=1
  fi

  if [[ -n "${BIN_DIR}" ]]; then
    if [[ ! -d "${BIN_DIR}" ]]; then
      echo "install.sh: --bin-dir is not a directory: ${BIN_DIR}" >&2
      missing=1
    elif [[ ! -x "${BIN_DIR}/aihub" ]] || [[ ! -x "${BIN_DIR}/aihubd" ]]; then
      echo "install.sh: --bin-dir must contain executable aihub and aihubd" >&2
      missing=1
    fi
  else
    local min_rust
    min_rust="$(read_rust_version_ms)"
    if ! need_cmd git 'Install Xcode Command Line Tools or Git: xcode-select --install'; then
      missing=1
    fi
    if ! need_cmd cargo 'Install Rust via https://rustup.rs/ then: rustup toolchain install 1.98.1'; then
      missing=1
    else
      local rustc_ver
      rustc_ver="$(rustc --version | awk '{print $2}')"
      if ! version_ge "${rustc_ver}" "${min_rust}"; then
        echo "install.sh: rustc ${rustc_ver} is older than workspace rust-version ${min_rust}" >&2
        echo "install.sh: run: rustup toolchain install ${min_rust} && rustup default ${min_rust}" >&2
        missing=1
      fi
    fi
  fi

  if ! need_cmd curl 'Install curl (included with macOS Command Line Tools): xcode-select --install'; then
    missing=1
  fi
  if ! need_cmd shasum 'shasum ships with macOS; reinstall Command Line Tools if absent.'; then
    missing=1
  fi

  if [[ "${missing}" -ne 0 ]]; then
    exit 1
  fi
}

install_aihub_binaries() {
  run mkdir -p "${LOCAL_BIN}"
  if [[ -n "${BIN_DIR}" ]]; then
    run install -m 0755 "${BIN_DIR}/aihub" "${LOCAL_BIN}/aihub"
    run install -m 0755 "${BIN_DIR}/aihubd" "${LOCAL_BIN}/aihubd"
    return 0
  fi
  run cargo install --locked --path "${ROOT}/aihub" --root "${LOCAL_ROOT}"
  run cargo install --locked --path "${ROOT}/aihubd" --root "${LOCAL_ROOT}"
}

# --- ai-memory (transitional): delete this block when aihub-native memory ships ---
AI_MEMORY_VERSION="v2.2.2"

ai_memory_asset_name() {
  local arch
  arch="$(uname -m)"
  case "${arch}" in
    arm64) echo "ai-memory-macos-aarch64.tar.gz" ;;
    x86_64) echo "ai-memory-macos-x86_64.tar.gz" ;;
    *)
      echo "install.sh: unsupported macOS architecture: ${arch}" >&2
      exit 1
      ;;
  esac
}

ai_memory_expected_sha256() {
  local asset
  asset="$(ai_memory_asset_name)"
  case "${asset}" in
    ai-memory-macos-aarch64.tar.gz)
      echo "2ed926c1944b2e936e5570b03984cef46883841d1227001da5af4703f1cde523"
      ;;
    ai-memory-macos-x86_64.tar.gz)
      echo "b8e5554afa3ec8a304ef7da790159d1393bee5746bf30741dc54cb70850f9add"
      ;;
    *)
      echo "install.sh: no checksum for asset ${asset}" >&2
      exit 1
      ;;
  esac
}

# Downloads the pinned ai-memory release into ${1}, verifies its SHA-256, and
# extracts it there. Shared by install.sh and scripts/e2e-ai-memory.sh so both
# paths check the exact same bytes against the exact same pin.
ai_memory_fetch_verified() {
  local dest_dir="$1"
  local asset url base expected
  asset="$(ai_memory_asset_name)"
  base="https://github.com/akitaonrails/ai-memory/releases/download/${AI_MEMORY_VERSION}"
  url="${base}/${asset}"
  expected="$(ai_memory_expected_sha256)"

  if [[ -n "${AI_MEMORY_DOWNLOAD_CACHE:-}" ]] && [[ -f "${AI_MEMORY_DOWNLOAD_CACHE}/${asset}" ]]; then
    cp "${AI_MEMORY_DOWNLOAD_CACHE}/${asset}" "${dest_dir}/${asset}"
  else
    curl -fsSL -o "${dest_dir}/${asset}" "${url}"
    if [[ -n "${AI_MEMORY_DOWNLOAD_CACHE:-}" ]]; then
      mkdir -p "${AI_MEMORY_DOWNLOAD_CACHE}"
      cp "${dest_dir}/${asset}" "${AI_MEMORY_DOWNLOAD_CACHE}/${asset}"
    fi
  fi

  (
    cd "${dest_dir}"
    echo "${expected}  ${asset}" | shasum -a 256 -c -
  )
  tar -xzf "${dest_dir}/${asset}" -C "${dest_dir}"
}

install_ai_memory_transitional() {
  local asset tmpdir installed_by_us=0
  asset="$(ai_memory_asset_name)"

  if [[ -x "${LOCAL_BIN}/ai-memory" ]]; then
    installed_by_us=0
  elif [[ "${DRY_RUN}" -eq 1 ]]; then
    local base expected
    base="https://github.com/akitaonrails/ai-memory/releases/download/${AI_MEMORY_VERSION}"
    expected="$(ai_memory_expected_sha256)"
    log "[dry-run] curl -fsSL -o <tmpdir>/${asset} ${base}/${asset}"
    log "[dry-run] verify sha256 ${expected} ${asset}"
    log "[dry-run] tar -xzf <tmpdir>/${asset} -C <tmpdir>"
    log "[dry-run] install ai-memory binary to ${LOCAL_BIN}/ai-memory"
    log "[dry-run] write marker ${INSTALL_AI_MEMORY_MARKER}"
    installed_by_us=1
  else
    tmpdir="$(mktemp -d)"
    # shellcheck disable=SC2064
    trap "rm -rf '${tmpdir}'" RETURN
    ai_memory_fetch_verified "${tmpdir}"
    run mkdir -p "${LOCAL_BIN}"
    run install -m 0755 "${tmpdir}/ai-memory" "${LOCAL_BIN}/ai-memory"
    if [[ -d "${tmpdir}/hooks" ]]; then
      mkdir -p "${LOCAL_BIN}/../share/ai-memory"
      run rm -rf "${LOCAL_ROOT}/share/ai-memory/hooks"
      run cp -R "${tmpdir}/hooks" "${LOCAL_ROOT}/share/ai-memory/hooks"
    fi
    installed_by_us=1
  fi

  if [[ "${installed_by_us}" -eq 1 ]] && [[ "${DRY_RUN}" -eq 0 ]]; then
    run mkdir -p "${AIHUB_STATE_DIR}"
    run touch "${INSTALL_AI_MEMORY_MARKER}"
  fi

  local data_dir config_path
  data_dir="${HOME}/.local/share/ai-memory"
  config_path="${HOME}/.config/ai-memory/config.toml"

  if [[ "${DRY_RUN}" -eq 1 ]]; then
    log "[dry-run] mkdir -p ${data_dir} $(dirname "${config_path}")"
    log "[dry-run] ai-memory --data-dir ${data_dir} --config ${config_path} init (if not initialised)"
  else
    mkdir -p "${data_dir}" "$(dirname "${config_path}")"
    if [[ ! -f "${config_path}" ]] || [[ ! -d "${data_dir}" ]] || [[ -z "$(ls -A "${data_dir}" 2>/dev/null || true)" ]]; then
      "${LOCAL_BIN}/ai-memory" --data-dir "${data_dir}" --config "${config_path}" init
    fi
  fi
}
# --- end ai-memory (transitional) ---

LAUNCHD_PATH=""

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

resolve_launchd_harness_path() {
  local -a harness_names=(claude codex cursor-agent agy)
  local -a harness_dirs=()
  local name path
  for name in "${harness_names[@]}"; do
    if path="$(command -v "${name}" 2>/dev/null)"; then
      harness_dirs+=("$(dirname "${path}")")
    else
      warn "harness not found in installing shell PATH: ${name}"
    fi
  done

  local extra="/usr/bin:/bin:/usr/sbin:/sbin"
  if [[ "${#harness_dirs[@]}" -eq 0 ]]; then
    LAUNCHD_PATH="${extra}"
  else
    local unique
    unique="$(path_unique_dirs "${harness_dirs[@]}")"
    LAUNCHD_PATH="${unique}:${extra}"
  fi

  for name in "${harness_names[@]}"; do
    if path="$(command -v "${name}" 2>/dev/null)"; then
      if ! env -i PATH="${LAUNCHD_PATH}" /bin/sh -c "command -v ${name}" >/dev/null 2>&1; then
        echo "install.sh: harness ${name} resolves to ${path} but not under LaunchAgent PATH" >&2
        echo "install.sh: PATH=${LAUNCHD_PATH}" >&2
        exit 1
      fi
    fi
  done
}

plist_replace_string() {
  local file="$1"
  local key="$2"
  local value="$3"
  if plutil -replace "${key}" -string "${value}" "${file}" 2>/dev/null; then
    return 0
  fi
  plutil -insert "${key}" -string "${value}" "${file}"
}

install_plist_atomically() {
  local rendered="$1"
  local dest="$2"
  if ! plutil -lint "${rendered}" >/dev/null; then
    echo "install.sh: plist failed plutil -lint: ${rendered}" >&2
    plutil -lint "${rendered}" >&2 || true
    exit 1
  fi
  local dest_dir
  dest_dir="$(dirname "${dest}")"
  run mkdir -p "${dest_dir}"
  run mv "${rendered}" "${dest}"
}

render_memory_launchd_plist() {
  local template="$1"
  local dest="$2"
  local memory_bin="$3"
  local data_dir="${HOME}/.local/share/ai-memory"
  local config_path="${HOME}/.config/ai-memory/config.toml"
  local stdout_path="${LOG_DIR}/ai-memory.stdout.log"
  local stderr_path="${LOG_DIR}/ai-memory.stderr.log"

  if [[ "${DRY_RUN}" -eq 1 ]]; then
    log "[dry-run] render ${template} -> ${dest} (plutil)"
    return 0
  fi

  local tmp
  tmp="$(mktemp "${dest}.XXXXXX")"
  cp "${template}" "${tmp}"

  plist_replace_string "${tmp}" "ProgramArguments.0" "${memory_bin}"
  plist_replace_string "${tmp}" "ProgramArguments.8" "${data_dir}"
  plist_replace_string "${tmp}" "ProgramArguments.10" "${config_path}"
  plist_replace_string "${tmp}" "EnvironmentVariables.AI_MEMORY_BIND" "127.0.0.1:49374"
  plist_replace_string "${tmp}" "StandardOutPath" "${stdout_path}"
  plist_replace_string "${tmp}" "StandardErrorPath" "${stderr_path}"

  install_plist_atomically "${tmp}" "${dest}"
}

render_aihubd_launchd_plist() {
  local template="$1"
  local dest="$2"
  local aihubd_bin="$3"
  local stdout_path="${LOG_DIR}/aihubd.stdout.log"
  local stderr_path="${LOG_DIR}/aihubd.stderr.log"

  if [[ "${DRY_RUN}" -eq 1 ]]; then
    log "[dry-run] render ${template} -> ${dest} (plutil)"
    return 0
  fi

  local tmp
  tmp="$(mktemp "${dest}.XXXXXX")"
  cp "${template}" "${tmp}"

  plist_replace_string "${tmp}" "ProgramArguments.0" "${aihubd_bin}"
  plist_replace_string "${tmp}" "ProgramArguments.2" "${AIHUB_SOCKET}"
  plist_replace_string "${tmp}" "EnvironmentVariables.PATH" "${LAUNCHD_PATH}"
  plist_replace_string "${tmp}" "StandardOutPath" "${stdout_path}"
  plist_replace_string "${tmp}" "StandardErrorPath" "${stderr_path}"

  install_plist_atomically "${tmp}" "${dest}"
}

register_launchd_services() {
  local memory_bin="${LOCAL_BIN}/ai-memory"
  local aihubd_bin="${LOCAL_BIN}/aihubd"
  local memory_plist="${LAUNCH_AGENTS}/com.github.akitaonrails.ai-memory.plist"
  local aihubd_plist="${LAUNCH_AGENTS}/io.mathborgess.aihubd.plist"

  resolve_launchd_harness_path

  run mkdir -p "${LOG_DIR}" "${LAUNCH_AGENTS}"

  render_memory_launchd_plist \
    "${ROOT}/packaging/launchd/com.github.akitaonrails.ai-memory.plist" \
    "${memory_plist}" \
    "${memory_bin}"
  render_aihubd_launchd_plist \
    "${ROOT}/packaging/launchd/io.mathborgess.aihubd.plist" \
    "${aihubd_plist}" \
    "${aihubd_bin}"

  if [[ "${NO_START}" -eq 1 ]] || [[ "${DRY_RUN}" -eq 1 ]]; then
    if [[ "${NO_START}" -eq 1 ]]; then
      log "skipping launchctl bootstrap (--no-start)"
    fi
    return 0
  fi

  local uid domain
  uid="$(id -u)"
  domain="gui/${uid}"

  launchctl bootout "${domain}/${AI_MEMORY_LABEL}" 2>/dev/null || true
  launchctl bootout "${domain}/${AIHUBD_LABEL}" 2>/dev/null || true
  launchctl bootstrap "${domain}" "${memory_plist}"
  launchctl bootstrap "${domain}" "${aihubd_plist}"
}

wait_for_sidecar_ready() {
  local deadline=$((SECONDS + READINESS_DEADLINE_S))
  local body
  while ((SECONDS < deadline)); do
    if body="$(curl --max-time 2 -sS "http://127.0.0.1:49374/mcp" 2>/dev/null || true)"; then
      if grep -q '"jsonrpc"' <<<"${body}" && grep -q '"error"' <<<"${body}"; then
        return 0
      fi
    fi
    sleep 0.5
  done
  echo "install.sh: ai-memory not ready after ${READINESS_DEADLINE_S}s (GET /mcp)" >&2
  return 1
}

wait_for_aihubd_socket_ready() {
  local deadline=$((SECONDS + READINESS_DEADLINE_S))
  local mode
  while ((SECONDS < deadline)); do
    if [[ -S "${AIHUB_SOCKET}" ]]; then
      mode="$(stat -f '%Lp' "${AIHUB_SOCKET}" 2>/dev/null || true)"
      if [[ "${mode}" == "600" ]]; then
        return 0
      fi
    fi
    sleep 0.5
  done
  echo "install.sh: aihubd socket not ready after ${READINESS_DEADLINE_S}s: ${AIHUB_SOCKET}" >&2
  return 1
}

health_checks() {
  if [[ "${NO_START}" -eq 1 ]] || [[ "${DRY_RUN}" -eq 1 ]]; then
    if [[ "${NO_START}" -eq 1 ]]; then
      log "skipping readiness checks (--no-start)"
    fi
    return 0
  fi

  wait_for_sidecar_ready
  wait_for_aihubd_socket_ready
}

install_agent_hooks_opt_in() {
  if [[ "${WITH_AGENT_HOOKS}" -ne 1 ]]; then
    return 0
  fi

  local -a agents=(
    "claude-code"
    "codex"
    "cursor"
    "antigravity-cli"
  )
  local id
  for id in "${agents[@]}"; do
    run "${LOCAL_BIN}/ai-memory" install-mcp --client "${id}" --apply
    run "${LOCAL_BIN}/ai-memory" install-hooks --agent "${id}" --apply
  done
}

main() {
  check_prerequisites
  install_aihub_binaries
  install_ai_memory_transitional
  register_launchd_services
  health_checks
  install_agent_hooks_opt_in
  log "done"
}

# scripts/e2e-ai-memory.sh sources this file to reuse ai_memory_fetch_verified()
# and friends without running the real installer against the real $HOME.
if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
  main "$@"
fi
