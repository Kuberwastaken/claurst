#!/usr/bin/env bash

set -euo pipefail

PACKAGE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET_HOME="${TARGET_HOME:-${HOME}}"
STATE_DIR="${TARGET_HOME}/.claude/litellm"
CONFIG_DIR="${STATE_DIR}/configs"
LOG_DIR="${STATE_DIR}/logs"
LOCAL_BIN_DIR="${TARGET_HOME}/.local/bin"
ZSHRC_PATH="${TARGET_HOME}/.zshrc"
SOURCE_LINE='source ~/.claude/litellm/shell-functions.zsh'

link_into_place() {
  local source_path="$1"
  local target_path="$2"
  mkdir -p "$(dirname "${target_path}")"
  rm -f "${target_path}"
  ln -s "${source_path}" "${target_path}"
}

mkdir -p "${CONFIG_DIR}" "${LOG_DIR}" "${LOCAL_BIN_DIR}" "${TARGET_HOME}/.claude"

link_into_place "${PACKAGE_DIR}/README.md" "${STATE_DIR}/README.md"
link_into_place "${PACKAGE_DIR}/env.example" "${STATE_DIR}/env.example"
link_into_place "${PACKAGE_DIR}/mcp-model-router.py" "${STATE_DIR}/mcp-model-router.py"
link_into_place "${PACKAGE_DIR}/shell-functions.zsh" "${STATE_DIR}/shell-functions.zsh"
link_into_place "${PACKAGE_DIR}/start.sh" "${STATE_DIR}/start.sh"
link_into_place "${PACKAGE_DIR}/status.sh" "${STATE_DIR}/status.sh"
link_into_place "${PACKAGE_DIR}/stop.sh" "${STATE_DIR}/stop.sh"

link_into_place "${PACKAGE_DIR}/configs/local.yaml" "${CONFIG_DIR}/local.yaml"
link_into_place "${PACKAGE_DIR}/configs/llamacpp.yaml" "${CONFIG_DIR}/llamacpp.yaml"
link_into_place "${PACKAGE_DIR}/configs/hybrid.yaml" "${CONFIG_DIR}/hybrid.yaml"
link_into_place "${PACKAGE_DIR}/configs/cloud.yaml" "${CONFIG_DIR}/cloud.yaml"

link_into_place "${PACKAGE_DIR}/bin/claude-switch" "${LOCAL_BIN_DIR}/claude-switch"
link_into_place "${PACKAGE_DIR}/bin/cursor-backend" "${LOCAL_BIN_DIR}/cursor-backend"
link_into_place "${PACKAGE_DIR}/bin/cursor-cc" "${LOCAL_BIN_DIR}/cursor-cc"

chmod +x \
  "${PACKAGE_DIR}/install.sh" \
  "${PACKAGE_DIR}/start.sh" \
  "${PACKAGE_DIR}/status.sh" \
  "${PACKAGE_DIR}/stop.sh" \
  "${PACKAGE_DIR}/bin/claude-switch" \
  "${PACKAGE_DIR}/bin/cursor-backend" \
  "${PACKAGE_DIR}/bin/cursor-cc" \
  "${PACKAGE_DIR}/mcp-model-router.py"

if [ ! -f "${STATE_DIR}/current" ]; then
  echo "local" > "${STATE_DIR}/current"
fi

touch "${ZSHRC_PATH}"
if ! grep -Fqx "${SOURCE_LINE}" "${ZSHRC_PATH}"; then
  printf '\n%s\n' "${SOURCE_LINE}" >> "${ZSHRC_PATH}"
fi

cat <<EOF
Installed the multi-backend router into:
  ${STATE_DIR}

Linked executables into:
  ${LOCAL_BIN_DIR}

Next steps:
  1. pip install "litellm[proxy]" mcp httpx
  2. source "${ZSHRC_PATH}"
  3. optional: cp "${STATE_DIR}/env.example" "${STATE_DIR}/env"
  4. claude-switch local
  5. cc

Optional MCP setup:
  Claude Code:
    claude mcp add -s user model-router python3 ~/.claude/litellm/mcp-model-router.py

  Cursor:
    Add ~/.claude/litellm/mcp-model-router.py to ~/.cursor/mcp.json as documented in:
    ${STATE_DIR}/README.md
EOF
