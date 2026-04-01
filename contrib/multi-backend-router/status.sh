#!/usr/bin/env bash

set -euo pipefail

STATE_DIR="${HOME}/.claude/litellm"
LOG_DIR="${STATE_DIR}/logs"
CURRENT_FILE="${STATE_DIR}/current"
LITELLM_PID_FILE="${STATE_DIR}/litellm.pid"
LLAMACPP_PID_FILE="${STATE_DIR}/llamacpp.pid"
LLAMACPP_PORT_FILE="${STATE_DIR}/llamacpp.port"
OLLAMA_PID_FILE="${STATE_DIR}/ollama.pid"

load_env_file() {
  local env_file="$1"
  if [ -f "${env_file}" ]; then
    set -a
    # shellcheck disable=SC1090
    . "${env_file}"
    set +a
  fi
}

load_env_file "${STATE_DIR}/env"
load_env_file "${STATE_DIR}/env.local"

LITELLM_HOST="${LITELLM_HOST:-127.0.0.1}"
LITELLM_PORT="${LITELLM_PORT:-4000}"
OLLAMA_HOST="${OLLAMA_HOST:-127.0.0.1}"
OLLAMA_PORT="${OLLAMA_PORT:-11434}"

MODE="anthropic"
if [ -f "${CURRENT_FILE}" ]; then
  MODE="$(cat "${CURRENT_FILE}")"
fi

port_pid() {
  lsof -tiTCP:"$1" -sTCP:LISTEN 2>/dev/null | head -n 1 || true
}

print_process_status() {
  local label="$1"
  local port="$2"
  local url="$3"
  local pid
  pid="$(port_pid "${port}")"
  if curl -fsS --max-time 2 "${url}" >/dev/null 2>&1; then
    echo "${label}: running on port ${port}${pid:+ (PID ${pid})}"
  elif [ -n "${pid}" ]; then
    echo "${label}: listening on port ${port} but health check failed (PID ${pid})"
  else
    echo "${label}: not running"
  fi
}

print_key_status() {
  local name="$1"
  if [ -n "${!name:-}" ]; then
    echo "  ${name}: set"
  else
    echo "  ${name}: not set"
  fi
}

echo "Mode: ${MODE}"
if [ "${MODE}" = "local" ] || [ "${MODE}" = "llamacpp" ] || [ "${MODE}" = "hybrid" ] || [ "${MODE}" = "cloud" ]; then
  echo "Config: ${STATE_DIR}/configs/${MODE}.yaml"
fi
echo

if [ "${MODE}" = "hybrid" ] || [ "${MODE}" = "cloud" ]; then
  echo "Cloud API keys:"
  print_key_status "DEEPSEEK_API_KEY"
  print_key_status "TOGETHER_API_KEY"
  print_key_status "GEMINI_API_KEY"
  print_key_status "GROQ_API_KEY"
  print_key_status "MOONSHOT_API_KEY"
  print_key_status "KIMI_API_KEY"
  echo
fi

if [ "${MODE}" = "moonshot" ]; then
  echo "Moonshot direct mode:"
  print_key_status "MOONSHOT_API_KEY"
  print_key_status "KIMI_API_KEY"
  echo "  base_url: ${MOONSHOT_BASE_URL:-https://api.moonshot.ai/anthropic}"
  echo "  model: ${MOONSHOT_MODEL:-kimi-k2.5}"
  echo
fi

print_process_status "LiteLLM" "${LITELLM_PORT}" "http://${LITELLM_HOST}:${LITELLM_PORT}/health/liveliness"

LLAMACPP_PORT=""
if [ -f "${LLAMACPP_PORT_FILE}" ]; then
  LLAMACPP_PORT="$(cat "${LLAMACPP_PORT_FILE}" 2>/dev/null || true)"
fi
if [ -n "${LLAMACPP_PORT}" ]; then
  print_process_status "llama-server" "${LLAMACPP_PORT}" "http://127.0.0.1:${LLAMACPP_PORT}/v1/models"
else
  echo "llama-server: no active port recorded"
fi

if curl -fsS --max-time 2 "http://${OLLAMA_HOST}:${OLLAMA_PORT}/api/tags" >/dev/null 2>&1; then
  echo "Ollama: running on port ${OLLAMA_PORT}${OLLAMA_PID_FILE:+}"
  echo "Loaded Ollama models:"
  OLLAMA_PS="$(curl -fsS --max-time 2 "http://${OLLAMA_HOST}:${OLLAMA_PORT}/api/ps" 2>/dev/null || true)"
  if [ -z "${OLLAMA_PS}" ]; then
    echo "  unable to read /api/ps"
  else
    python3 -c '
import json
import sys

try:
    payload = json.load(sys.stdin)
except Exception:
    print("  unable to parse /api/ps")
    raise SystemExit(0)

models = payload.get("models") or []
if not models:
    print("  none currently loaded")
else:
    for model in models:
        name = model.get("name", "unknown")
        size = model.get("size_vram") or model.get("size") or "unknown-size"
        print(f"  {name} ({size})")
' <<<"${OLLAMA_PS}"
  fi
else
  echo "Ollama: not running"
fi

echo
echo "PID files:"
echo "  LiteLLM: ${LITELLM_PID_FILE}"
echo "  llama-server: ${LLAMACPP_PID_FILE}"
echo "  Ollama: ${OLLAMA_PID_FILE}"
echo

if [ -f "${LOG_DIR}/litellm.log" ]; then
  echo "Last 20 LiteLLM log lines:"
  tail -n 20 "${LOG_DIR}/litellm.log"
else
  echo "No LiteLLM log yet."
fi

echo
echo "Examples:"
echo "  claude-switch local      # Full local via Ollama"
echo "  claude-switch llamacpp   # Qwen via llama.cpp"
echo "  claude-switch hybrid     # Local primary, DeepSeek fallback"
echo "  claude-switch cloud      # Cloud-only cheap long context"
echo "  claude-switch moonshot   # Direct Kimi K2.5 / K2 Thinking via Moonshot"
echo "  claude-switch anthropic  # Direct Anthropic backend"
echo "  cp ~/.claude/litellm/env.example ~/.claude/litellm/env"
echo "  cc                       # Interactive Claude Code with current mode"
echo "  cc -p \"your prompt\"     # Non-interactive Claude Code with current mode"
