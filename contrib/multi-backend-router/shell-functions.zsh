export CLAUDE_LITELLM_HOME="${HOME}/.claude/litellm"

_cc_proxy_ready() {
  curl -fsS --max-time 2 "http://127.0.0.1:4000/health/liveliness" >/dev/null 2>&1
}

_cc_load_env_files() {
  local state_dir="${CLAUDE_LITELLM_HOME}"
  local env_file
  for env_file in "${state_dir}/env" "${state_dir}/env.local"; do
    if [ -f "${env_file}" ]; then
      set -a
      # shellcheck disable=SC1090
      . "${env_file}"
      set +a
    fi
  done
}

_cc_mode_needs_local_compat() {
  case "$1" in
    local|llamacpp|hybrid)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

cc() {
  local state_dir="${CLAUDE_LITELLM_HOME}"
  local mode="anthropic"
  local -a env_args
  env_args=()

  if [ -f "${state_dir}/current" ]; then
    mode="$(cat "${state_dir}/current")"
  fi

  if [ "${mode}" = "anthropic" ]; then
    command claude "$@"
    return
  fi

  if [ "${mode}" = "moonshot" ]; then
    _cc_load_env_files

    local moonshot_key="${MOONSHOT_API_KEY:-${KIMI_API_KEY:-}}"
    if [ -z "${moonshot_key}" ]; then
      echo "moonshot mode needs MOONSHOT_API_KEY (or KIMI_API_KEY) in ~/.claude/litellm/env" >&2
      return 1
    fi

    env_args=(
      "ANTHROPIC_BASE_URL=${MOONSHOT_BASE_URL:-https://api.moonshot.ai/anthropic}"
      "ANTHROPIC_API_KEY=${moonshot_key}"
      "ANTHROPIC_AUTH_TOKEN=${moonshot_key}"
      "ANTHROPIC_MODEL=${MOONSHOT_MODEL:-kimi-k2.5}"
      "ANTHROPIC_DEFAULT_OPUS_MODEL=${MOONSHOT_OPUS_MODEL:-${MOONSHOT_MODEL:-kimi-k2.5}}"
      "ANTHROPIC_DEFAULT_SONNET_MODEL=${MOONSHOT_SONNET_MODEL:-${MOONSHOT_MODEL:-kimi-k2.5}}"
      "ANTHROPIC_DEFAULT_HAIKU_MODEL=${MOONSHOT_HAIKU_MODEL:-${MOONSHOT_MODEL:-kimi-k2.5}}"
      "CLAUDE_CODE_SUBAGENT_MODEL=${MOONSHOT_SUBAGENT_MODEL:-${MOONSHOT_MODEL:-kimi-k2.5}}"
      "ENABLE_TOOL_SEARCH=${MOONSHOT_ENABLE_TOOL_SEARCH:-false}"
    )

    command env \
      -u CLAUDE_CODE_OAUTH_TOKEN \
      -u CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR \
      "${env_args[@]}" \
      claude "$@"
    return
  fi

  if ! _cc_proxy_ready; then
    echo "Starting ${mode} backend..." >&2
    "${state_dir}/start.sh" "${mode}" >/dev/null || return $?
  fi

  env_args=(
    "ANTHROPIC_BASE_URL=http://127.0.0.1:4000"
    "ANTHROPIC_API_KEY=${CLAUDE_PROXY_API_KEY:-sk-litellm}"
    "ANTHROPIC_AUTH_TOKEN=${CLAUDE_PROXY_API_KEY:-sk-litellm}"
  )

  if _cc_mode_needs_local_compat "${mode}"; then
    env_args+=("CLAUDE_CODE_DISABLE_THINKING=1")
  fi

  command env \
    -u CLAUDE_CODE_OAUTH_TOKEN \
    -u CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR \
    "${env_args[@]}" \
    claude "$@"
}

claude-status() {
  "${CLAUDE_LITELLM_HOME}/status.sh"
}

alias cs="claude-switch"
alias cb="cursor-backend"
