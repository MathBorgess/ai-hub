#!/usr/bin/env bash
# Install aihub, aihubd, ai-memory (transitional), and macOS LaunchAgents.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT

DRY_RUN=0
WITH_AGENT_HOOKS=0

LOCAL_BIN="${HOME}/.local/bin"
LOCAL_ROOT="${HOME}/.local"
AIHUB_SOCKET="${HOME}/.local/share/aihub/aihub.sock"
LOG_DIR="${HOME}/Library/Logs/aihub"
LAUNCH_AGENTS="${HOME}/Library/LaunchAgents"

AI_MEMORY_LABEL="com.github.akitaonrails.ai-memory"
AIHUBD_LABEL="io.mathborgess.aihubd"

usage() {
  cat <<'EOF'
Usage: install.sh [--dry-run] [--with-agent-hooks]

  --dry-run            Print planned actions without changing the system.
  --with-agent-hooks   Run ai-memory install-mcp/install-hooks for supported agents.
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
  local min_rust
  min_rust="$(read_rust_version_ms)"

  if ! need_cmd git 'Install Xcode Command Line Tools or Git: xcode-select --install'; then
    missing=1
  fi
  if ! need_cmd curl 'Install curl (included with macOS Command Line Tools): xcode-select --install'; then
    missing=1
  fi
  if ! need_cmd shasum 'shasum ships with macOS; reinstall Command Line Tools if absent.'; then
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

  if [[ "${missing}" -ne 0 ]]; then
    exit 1
  fi
}

install_aihub_binaries() {
  run mkdir -p "${LOCAL_BIN}"
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
  local asset url base sha_url expected
  asset="$(ai_memory_asset_name)"
  base="https://github.com/akitaonrails/ai-memory/releases/download/${AI_MEMORY_VERSION}"
  url="${base}/${asset}"
  sha_url="${base}/${asset}.sha256"
  expected="$(ai_memory_expected_sha256)"

  curl -fsSL -o "${dest_dir}/${asset}" "${url}"
  curl -fsSL -o "${dest_dir}/${asset}.sha256" "${sha_url}"
  (
    cd "${dest_dir}"
    echo "${expected}  ${asset}" | shasum -a 256 -c -
  )
  tar -xzf "${dest_dir}/${asset}" -C "${dest_dir}"
}

install_ai_memory_transitional() {
  local asset tmpdir
  asset="$(ai_memory_asset_name)"

  if [[ "${DRY_RUN}" -eq 1 ]]; then
    local base sha_url expected
    base="https://github.com/akitaonrails/ai-memory/releases/download/${AI_MEMORY_VERSION}"
    sha_url="${base}/${asset}.sha256"
    expected="$(ai_memory_expected_sha256)"
    log "[dry-run] curl -fsSL -o <tmpdir>/${asset} ${base}/${asset}"
    log "[dry-run] curl -fsSL -o <tmpdir>/${asset}.sha256 ${sha_url}"
    log "[dry-run] verify sha256 ${expected} ${asset}"
    log "[dry-run] tar -xzf <tmpdir>/${asset} -C <tmpdir>"
    log "[dry-run] install ai-memory binary to ${LOCAL_BIN}/ai-memory"
  else
    tmpdir="$(mktemp -d)"
    # shellcheck disable=SC2064
    trap "rm -rf '${tmpdir}'" RETURN
    ai_memory_fetch_verified "${tmpdir}"
    install -m 0755 "${tmpdir}/ai-memory" "${LOCAL_BIN}/ai-memory"
    if [[ -d "${tmpdir}/hooks" ]]; then
      mkdir -p "${LOCAL_BIN}/../share/ai-memory"
      run rm -rf "${LOCAL_ROOT}/share/ai-memory/hooks"
      run cp -R "${tmpdir}/hooks" "${LOCAL_ROOT}/share/ai-memory/hooks"
    fi
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

render_launchd_plist() {
  local template="$1"
  local dest="$2"
  local memory_bin="$3"
  local aihubd_bin="$4"
  if [[ "${DRY_RUN}" -eq 1 ]]; then
    log "[dry-run] render ${template} -> ${dest}"
    return 0
  fi
  case "$(basename "${template}")" in
    com.github.akitaonrails.ai-memory.plist)
      sed -e "s|__AI_MEMORY_BIN__|${memory_bin}|g" \
        -e "s|__HOME__|${HOME}|g" \
        "${template}" >"${dest}"
      ;;
    io.mathborgess.aihubd.plist)
      sed -e "s|__AIHUBD_BIN__|${aihubd_bin}|g" \
        -e "s|__HOME__|${HOME}|g" \
        "${template}" >"${dest}"
      ;;
    *)
      echo "install.sh: unknown launchd template: ${template}" >&2
      exit 1
      ;;
  esac
}

register_launchd_services() {
  local memory_bin="${LOCAL_BIN}/ai-memory"
  local aihubd_bin="${LOCAL_BIN}/aihubd"
  local memory_plist="${LAUNCH_AGENTS}/com.github.akitaonrails.ai-memory.plist"
  local aihubd_plist="${LAUNCH_AGENTS}/io.mathborgess.aihubd.plist"

  run mkdir -p "${LOG_DIR}" "${LAUNCH_AGENTS}"

  render_launchd_plist \
    "${ROOT}/packaging/launchd/com.github.akitaonrails.ai-memory.plist" \
    "${memory_plist}" \
    "${memory_bin}" \
    "${aihubd_bin}"
  render_launchd_plist \
    "${ROOT}/packaging/launchd/io.mathborgess.aihubd.plist" \
    "${aihubd_plist}" \
    "${memory_bin}" \
    "${aihubd_bin}"

  local uid domain
  uid="$(id -u)"
  domain="gui/${uid}"

  if [[ "${DRY_RUN}" -eq 1 ]]; then
    log "[dry-run] launchctl bootout ${domain}/${AI_MEMORY_LABEL} (ignore errors)"
    log "[dry-run] launchctl bootout ${domain}/${AIHUBD_LABEL} (ignore errors)"
    log "[dry-run] launchctl bootstrap ${domain} ${memory_plist}"
    log "[dry-run] launchctl bootstrap ${domain} ${aihubd_plist}"
    return 0
  fi

  launchctl bootout "${domain}/${AI_MEMORY_LABEL}" 2>/dev/null || true
  launchctl bootout "${domain}/${AIHUBD_LABEL}" 2>/dev/null || true
  launchctl bootstrap "${domain}" "${memory_plist}"
  launchctl bootstrap "${domain}" "${aihubd_plist}"
}

health_checks() {
  if [[ "${DRY_RUN}" -eq 1 ]]; then
    log "[dry-run] curl -sS http://127.0.0.1:49374/mcp (expect JSON-RPC error body)"
    log "[dry-run] test socket ${AIHUB_SOCKET} mode 0600"
    return 0
  fi

  local body
  body="$(curl -sS "http://127.0.0.1:49374/mcp" || true)"
  if ! grep -q '"jsonrpc"' <<<"${body}" || ! grep -q '"error"' <<<"${body}"; then
    echo "install.sh: ai-memory health check failed on GET /mcp" >&2
    echo "install.sh: response: ${body}" >&2
    exit 1
  fi

  if [[ ! -S "${AIHUB_SOCKET}" ]]; then
    echo "install.sh: aihubd socket missing: ${AIHUB_SOCKET}" >&2
    exit 1
  fi
  local mode
  mode="$(stat -f '%Lp' "${AIHUB_SOCKET}")"
  if [[ "${mode}" != "600" ]]; then
    echo "install.sh: aihubd socket mode is ${mode}, expected 600" >&2
    exit 1
  fi
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
