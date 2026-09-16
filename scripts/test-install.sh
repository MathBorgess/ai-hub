#!/usr/bin/env bash
# Isolated installer regression tests (N7/N8/N9, uninstall/purge). Never touches real $HOME.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly ROOT

fail() {
  echo "test-install.sh: $*" >&2
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

make_fake_binaries() {
  local dir="$1"
  mkdir -p "${dir}"
  for name in aihub aihubd; do
    cat >"${dir}/${name}" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
    chmod +x "${dir}/${name}"
  done
}

setup_isolated_home() {
  local parent
  parent="$(mktemp -d "${TMPDIR:-/tmp}/aihub install & test.XXXXXX")"
  CLEANUP_DIRS+=("${parent}")
  ISOLATED_HOME="${parent}/user home"
  mkdir -p "${ISOLATED_HOME}"
  LAUNCHCTL_LOG="${ISOLATED_HOME}/launchctl.log"
  : >"${LAUNCHCTL_LOG}"
  STUB_BIN="${ISOLATED_HOME}/stub-bin"
  mkdir -p "${STUB_BIN}"
  cat >"${STUB_BIN}/launchctl" <<EOF
#!/usr/bin/env bash
printf '%s\n' "\$*" >> "${LAUNCHCTL_LOG}"
exit 0
EOF
  chmod +x "${STUB_BIN}/launchctl"

  FAKE_BIN_DIR="${ISOLATED_HOME}/prebuilt"
  make_fake_binaries "${FAKE_BIN_DIR}"

  AI_MEMORY_CACHE="${ISOLATED_HOME}/ai-memory-cache"
  mkdir -p "${AI_MEMORY_CACHE}"
  export AI_MEMORY_DOWNLOAD_CACHE="${AI_MEMORY_CACHE}"
}

run_install() {
  HOME="${ISOLATED_HOME}" PATH="${STUB_BIN}:${PATH}" \
    "${ROOT}/scripts/install.sh" --no-start --bin-dir "${FAKE_BIN_DIR}"
}

run_uninstall() {
  HOME="${ISOLATED_HOME}" PATH="${STUB_BIN}:${PATH}" \
    "${ROOT}/scripts/uninstall.sh" "$@"
}

lint_plists() {
  plutil -lint "${ISOLATED_HOME}/Library/LaunchAgents/com.github.akitaonrails.ai-memory.plist"
  plutil -lint "${ISOLATED_HOME}/Library/LaunchAgents/io.mathborgess.aihubd.plist"
}

# n8: home paths with '&' and spaces appear literally in rendered plists.
assert_home_paths_rendered() {
  local socket_path="${ISOLATED_HOME}/.local/share/aihub/aihub.sock"
  local got
  got="$(plutil -extract ProgramArguments.2 raw -o - \
    "${ISOLATED_HOME}/Library/LaunchAgents/io.mathborgess.aihubd.plist")"
  assert_eq "${got}" "${socket_path}" "n8 aihubd socket path"
}

# n7: LaunchAgent PATH includes a harness directory from the installing shell.
test_n7_harness_path_in_plist() {
  local harness_dir="${ISOLATED_HOME}/harness-bin"
  mkdir -p "${harness_dir}"
  cat >"${harness_dir}/claude" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
  chmod +x "${harness_dir}/claude"

  HOME="${ISOLATED_HOME}" PATH="${STUB_BIN}:${harness_dir}:${PATH}" \
    "${ROOT}/scripts/install.sh" --no-start --bin-dir "${FAKE_BIN_DIR}"

  local launchd_path
  launchd_path="$(plutil -extract EnvironmentVariables.PATH raw -o - \
    "${ISOLATED_HOME}/Library/LaunchAgents/io.mathborgess.aihubd.plist")"
  case ":${launchd_path}:" in
    *":${harness_dir}:"*) ;;
    *) fail "n7: expected harness dir in LaunchAgent PATH, got ${launchd_path}" ;;
  esac

  if ! env -i PATH="${launchd_path}" /bin/sh -c 'command -v claude' >/dev/null 2>&1; then
    fail "n7: claude not resolved under plist PATH"
  fi
}

# n9: sidecar bind env and --no-start skips launchctl.
test_n9_bind_and_no_start() {
  assert_eq "$(plutil -extract EnvironmentVariables.AI_MEMORY_BIND raw -o - \
    "${ISOLATED_HOME}/Library/LaunchAgents/com.github.akitaonrails.ai-memory.plist")" \
    "127.0.0.1:49374" "n9 AI_MEMORY_BIND"
  if [[ -s "${LAUNCHCTL_LOG}" ]]; then
    fail "n9: --no-start must not invoke launchctl"
  fi
}

test_fresh_install() {
  setup_isolated_home
  run_install
  lint_plists
  assert_home_paths_rendered
  test_n9_bind_and_no_start
  assert_path_exists "${ISOLATED_HOME}/.local/share/aihub/.installed-ai-memory-by-aihub" \
    "fresh install writes ai-memory marker"
  assert_path_exists "${ISOLATED_HOME}/.local/bin/aihub" "aihub binary"
  assert_path_exists "${ISOLATED_HOME}/.local/share/ai-memory" "ai-memory data dir"
}

test_repeat_install_idempotent() {
  run_install
  run_install
  lint_plists
}

test_uninstall_keeps_ai_memory_data() {
  echo "keep-me" >"${ISOLATED_HOME}/.local/share/ai-memory/preserve.txt"
  run_uninstall
  assert_path_exists "${ISOLATED_HOME}/.local/share/ai-memory/preserve.txt" \
    "uninstall without --purge-ai-memory keeps ai-memory data"
  assert_path_missing "${ISOLATED_HOME}/.local/bin/aihub" "aihub removed"
}

test_purge_surfaces() {
  run_install
  mkdir -p \
    "${ISOLATED_HOME}/.local/share/aihub" \
    "${ISOLATED_HOME}/.config/ai-memory" \
    "${ISOLATED_HOME}/Library/Application Support/ai-memory" \
    "${ISOLATED_HOME}/.local/share/ai-memory-hooks"
  touch \
    "${ISOLATED_HOME}/.local/share/aihub/spool" \
    "${ISOLATED_HOME}/.config/ai-memory/config.toml" \
    "${ISOLATED_HOME}/Library/Application Support/ai-memory/x" \
    "${ISOLATED_HOME}/.local/share/ai-memory-hooks/h"

  run_uninstall --purge --purge-ai-memory

  assert_path_missing "${ISOLATED_HOME}/.local/share/aihub" "purge removes aihub data"
  assert_path_missing "${ISOLATED_HOME}/.config/ai-memory" "purge-ai-memory removes config"
  assert_path_missing "${ISOLATED_HOME}/Library/Application Support/ai-memory" \
    "purge-ai-memory removes Application Support"
  assert_path_missing "${ISOLATED_HOME}/.local/share/ai-memory-hooks" \
    "purge-ai-memory removes hooks path"
  assert_path_missing "${ISOLATED_HOME}/.local/share/ai-memory" \
    "purge-ai-memory removes share/ai-memory"
}

test_preexisting_ai_memory_untouchable() {
  setup_isolated_home
  mkdir -p "${ISOLATED_HOME}/.local/bin" \
    "${ISOLATED_HOME}/.local/share/ai-memory" \
    "${ISOLATED_HOME}/.config/ai-memory"
  cat >"${ISOLATED_HOME}/.local/bin/ai-memory" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' 'preexisting-binary'
exit 0
EOF
  chmod +x "${ISOLATED_HOME}/.local/bin/ai-memory"
  echo "seed" >"${ISOLATED_HOME}/.local/share/ai-memory/existing.txt"
  echo 'bind = "127.0.0.1:49374"' >"${ISOLATED_HOME}/.config/ai-memory/config.toml"

  run_install
  assert_path_missing "${ISOLATED_HOME}/.local/share/aihub/.installed-ai-memory-by-aihub" \
    "pre-existing ai-memory must not write marker"
  assert_eq "$(cat "${ISOLATED_HOME}/.local/bin/ai-memory" | head -n1)" "#!/usr/bin/env bash" \
    "install must not replace pre-existing ai-memory"

  run_uninstall
  assert_eq "$( "${ISOLATED_HOME}/.local/bin/ai-memory" )" "preexisting-binary" \
    "uninstall must leave pre-existing ai-memory"
}

main() {
  test_fresh_install
  test_n7_harness_path_in_plist
  test_repeat_install_idempotent
  test_uninstall_keeps_ai_memory_data
  test_purge_surfaces
  test_preexisting_ai_memory_untouchable
  echo "test-install.sh: all cases passed"
}

main "$@"
