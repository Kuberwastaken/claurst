#!/usr/bin/env bash

set -euo pipefail

STATE_DIR="${HOME}/.claude/litellm"
CONFIG_DIR="${STATE_DIR}/configs"
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

MASTER_KEY="${CLAUDE_LITELLM_MASTER_KEY:-sk-litellm-local}"
LITELLM_HOST="${LITELLM_HOST:-127.0.0.1}"
LITELLM_PORT="${LITELLM_PORT:-4000}"
OLLAMA_HOST="${OLLAMA_HOST:-127.0.0.1}"
OLLAMA_PORT="${OLLAMA_PORT:-11434}"
OLLAMA_CONTEXT_LENGTH="${OLLAMA_CONTEXT_LENGTH:-32768}"
LLAMACPP_HOST="${LLAMACPP_HOST:-127.0.0.1}"
LLAMACPP_PORT="${LLAMACPP_PORT:-8080}"
LLAMACPP_FALLBACK_PORT="${LLAMACPP_FALLBACK_PORT:-18080}"
LLAMACPP_CTX_SIZE="${LLAMACPP_CTX_SIZE:-65536}"
LLAMACPP_NGL="${LLAMACPP_NGL:-99}"
LLAMACPP_ALIAS="${LLAMACPP_ALIAS:-qwen3-coder-30b}"
LLAMACPP_API_KEY="${LLAMACPP_API_KEY:-sk-dummy}"
LLAMACPP_MODEL="${LLAMACPP_MODEL:-${HOME}/.ollama/models/blobs/sha256-1194192cf2a187eb02722edcc3f77b11d21f537048ce04b67ccf8ba78863006a}"

MODE="${1:-}"
if [ -z "${MODE}" ] && [ -f "${CURRENT_FILE}" ]; then
  MODE="$(cat "${CURRENT_FILE}")"
fi
MODE="${MODE:-local}"

OLLAMA_BIN="${OLLAMA_BIN:-/opt/homebrew/bin/ollama}"
LLAMACPP_BIN="${LLAMACPP_BIN:-/opt/homebrew/bin/llama-server}"
LITELLM_BIN="${LITELLM_BIN:-}"
if [ -z "${LITELLM_BIN}" ]; then
  LITELLM_BIN="$(command -v litellm || true)"
fi

mkdir -p "${STATE_DIR}" "${CONFIG_DIR}" "${LOG_DIR}"

port_pid() {
  lsof -tiTCP:"$1" -sTCP:LISTEN 2>/dev/null | head -n 1 || true
}

proc_cmd() {
  ps -p "$1" -o command= 2>/dev/null || true
}

terminate_pid() {
  local pid="$1"
  kill "${pid}" >/dev/null 2>&1 || true
  for _ in {1..20}; do
    if ! kill -0 "${pid}" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.25
  done
  kill -9 "${pid}" >/dev/null 2>&1 || true
}

stop_pid_file() {
  local pid_file="$1"
  local match="$2"
  local label="$3"
  if [ ! -f "${pid_file}" ]; then
    return 0
  fi

  local pid
  pid="$(cat "${pid_file}" 2>/dev/null || true)"
  if [ -n "${pid}" ]; then
    local cmd
    cmd="$(proc_cmd "${pid}")"
    if [ -n "${cmd}" ] && [[ "${cmd}" == *"${match}"* ]]; then
      echo "Stopping ${label} (PID ${pid})..."
      terminate_pid "${pid}"
    fi
  fi

  rm -f "${pid_file}"
}

stop_port_if_matches() {
  local port="$1"
  local match="$2"
  local label="$3"
  local pid
  pid="$(port_pid "${port}")"
  if [ -z "${pid}" ]; then
    return 0
  fi

  local cmd
  cmd="$(proc_cmd "${pid}")"
  if [[ "${cmd}" == *"${match}"* ]]; then
    echo "Stopping ${label} on port ${port} (PID ${pid})..."
    terminate_pid "${pid}"
    return 0
  fi

  echo "${label} port ${port} is occupied by a different process: ${cmd}" >&2
  return 1
}

wait_http() {
  local url="$1"
  local attempts="$2"
  local sleep_seconds="$3"
  local i
  for ((i = 1; i <= attempts; i++)); do
    if curl -fsS --max-time 2 "${url}" >/dev/null 2>&1; then
      return 0
    fi
    sleep "${sleep_seconds}"
  done
  return 1
}

spawn_detached() {
  local pid_file="$1"
  local log_file="$2"
  shift 2

  python3 - "${pid_file}" "${log_file}" "$@" <<'PY'
import os
import subprocess
import sys

pid_file, log_file, *cmd = sys.argv[1:]
env = os.environ.copy()

with open(log_file, "ab", buffering=0) as log_handle:
    proc = subprocess.Popen(
        cmd,
        stdin=subprocess.DEVNULL,
        stdout=log_handle,
        stderr=log_handle,
        start_new_session=True,
        env=env,
    )

with open(pid_file, "w", encoding="utf-8") as fh:
    fh.write(str(proc.pid))
PY
}

stop_owned_litellm() {
  stop_pid_file "${LITELLM_PID_FILE}" "litellm" "LiteLLM"
  stop_port_if_matches "${LITELLM_PORT}" "litellm" "LiteLLM"
}

stop_owned_llamacpp() {
  stop_pid_file "${LLAMACPP_PID_FILE}" "llama-server" "llama-server"
  if [ -f "${LLAMACPP_PORT_FILE}" ]; then
    local port
    port="$(cat "${LLAMACPP_PORT_FILE}" 2>/dev/null || true)"
    if [ -n "${port}" ]; then
      stop_port_if_matches "${port}" "llama-server" "llama-server" || true
    fi
    rm -f "${LLAMACPP_PORT_FILE}"
  fi
}

stop_owned_ollama() {
  stop_pid_file "${OLLAMA_PID_FILE}" "ollama serve" "Ollama"
}

ensure_executable() {
  local path="$1"
  local label="$2"
  if [ ! -x "${path}" ]; then
    echo "${label} not found at ${path}" >&2
    exit 1
  fi
}

pick_llamacpp_port() {
  local candidate
  local pid
  local cmd

  for candidate in "${LLAMACPP_PORT}" "${LLAMACPP_FALLBACK_PORT}" 18081 18082 18083 8081 8082 8090; do
    pid="$(port_pid "${candidate}")"
    if [ -z "${pid}" ]; then
      printf '%s\n' "${candidate}"
      return 0
    fi

    cmd="$(proc_cmd "${pid}")"
    if [[ "${cmd}" == *"llama-server"* ]]; then
      printf '%s\n' "${candidate}"
      return 0
    fi
  done

  echo "No safe llama.cpp port is available. Free up 8080/18080/18081/18082/18083/8081/8082/8090 or set LLAMACPP_PORT." >&2
  exit 1
}

start_ollama_if_needed() {
  if [ "${MODE}" != "local" ] && [ "${MODE}" != "hybrid" ]; then
    return 0
  fi

  ensure_executable "${OLLAMA_BIN}" "ollama"

  if wait_http "http://${OLLAMA_HOST}:${OLLAMA_PORT}/api/tags" 1 1; then
    echo "Ollama is already responding on ${OLLAMA_HOST}:${OLLAMA_PORT}."
    return 0
  fi

  echo "Starting Ollama on ${OLLAMA_HOST}:${OLLAMA_PORT}..."
  OLLAMA_CONTEXT_LENGTH="${OLLAMA_CONTEXT_LENGTH}" \
    spawn_detached "${OLLAMA_PID_FILE}" "${LOG_DIR}/ollama.log" "${OLLAMA_BIN}" serve

  if ! wait_http "http://${OLLAMA_HOST}:${OLLAMA_PORT}/api/tags" 30 1; then
    echo "Ollama did not become ready. Check ${LOG_DIR}/ollama.log" >&2
    exit 1
  fi

  echo "Ollama is ready."
}

start_llamacpp_if_needed() {
  if [ "${MODE}" != "llamacpp" ]; then
    return 0
  fi

  ensure_executable "${LLAMACPP_BIN}" "llama-server"

  if [ ! -f "${LLAMACPP_MODEL}" ]; then
    echo "Missing llama.cpp model blob: ${LLAMACPP_MODEL}" >&2
    echo "Set LLAMACPP_MODEL in ~/.claude/litellm/env to point at your GGUF blob." >&2
    exit 1
  fi

  local port
  port="$(pick_llamacpp_port)"
  echo "${port}" > "${LLAMACPP_PORT_FILE}"

  if wait_http "http://${LLAMACPP_HOST}:${port}/v1/models" 1 1; then
    echo "llama-server is already responding on ${LLAMACPP_HOST}:${port}."
    return 0
  fi

  echo "Starting llama-server on ${LLAMACPP_HOST}:${port}..."
  spawn_detached "${LLAMACPP_PID_FILE}" "${LOG_DIR}/llama-server.log" \
    "${LLAMACPP_BIN}" \
    -m "${LLAMACPP_MODEL}" \
    --port "${port}" \
    --host "${LLAMACPP_HOST}" \
    --ctx-size "${LLAMACPP_CTX_SIZE}" \
    -ngl "${LLAMACPP_NGL}" \
    --jinja \
    --alias "${LLAMACPP_ALIAS}"

  if ! wait_http "http://${LLAMACPP_HOST}:${port}/v1/models" 180 1; then
    echo "llama-server did not become ready. Check ${LOG_DIR}/llama-server.log" >&2
    exit 1
  fi

  echo "llama-server is ready."
}

start_litellm() {
  local config_path="${CONFIG_DIR}/${MODE}.yaml"
  if [ ! -f "${config_path}" ]; then
    echo "Missing LiteLLM config: ${config_path}" >&2
    exit 1
  fi

  if [ -z "${LITELLM_BIN}" ] || [ ! -x "${LITELLM_BIN}" ]; then
    echo "litellm is not installed or not executable." >&2
    exit 1
  fi

  if [ "${MODE}" = "llamacpp" ]; then
    local port
    port="$(cat "${LLAMACPP_PORT_FILE}")"
    echo "Starting LiteLLM (${MODE}) on ${LITELLM_HOST}:${LITELLM_PORT} -> llama.cpp ${LLAMACPP_HOST}:${port}..."
    CLAUDE_LITELLM_MASTER_KEY="${MASTER_KEY}" \
      LLAMACPP_API_BASE="http://${LLAMACPP_HOST}:${port}/v1" \
      LLAMACPP_API_KEY="${LLAMACPP_API_KEY}" \
      spawn_detached "${LITELLM_PID_FILE}" "${LOG_DIR}/litellm.log" \
      "${LITELLM_BIN}" \
      --host "${LITELLM_HOST}" \
      --config "${config_path}" \
      --port "${LITELLM_PORT}"
  else
    echo "Starting LiteLLM (${MODE}) on ${LITELLM_HOST}:${LITELLM_PORT}..."
    CLAUDE_LITELLM_MASTER_KEY="${MASTER_KEY}" \
      spawn_detached "${LITELLM_PID_FILE}" "${LOG_DIR}/litellm.log" \
      "${LITELLM_BIN}" \
      --host "${LITELLM_HOST}" \
      --config "${config_path}" \
      --port "${LITELLM_PORT}"
  fi

  if ! wait_http "http://${LITELLM_HOST}:${LITELLM_PORT}/health/liveliness" 20 1; then
    echo "LiteLLM did not become ready. Check ${LOG_DIR}/litellm.log" >&2
    exit 1
  fi
}

if [ "${MODE}" = "anthropic" ]; then
  echo "anthropic mode does not start LiteLLM."
  exit 0
fi

if [ "${MODE}" != "local" ] && [ "${MODE}" != "llamacpp" ] && [ "${MODE}" != "hybrid" ] && [ "${MODE}" != "cloud" ]; then
  echo "Unknown mode: ${MODE}" >&2
  exit 1
fi

echo "${MODE}" > "${CURRENT_FILE}"

stop_owned_litellm
if [ "${MODE}" = "llamacpp" ]; then
  stop_owned_ollama
else
  stop_owned_llamacpp
fi

start_ollama_if_needed
start_llamacpp_if_needed
start_litellm

echo
echo "Mode: ${MODE}"
echo "LiteLLM: http://${LITELLM_HOST}:${LITELLM_PORT}"
if [ "${MODE}" = "llamacpp" ]; then
  echo "llama-server: http://${LLAMACPP_HOST}:$(cat "${LLAMACPP_PORT_FILE}")/v1"
fi
if [ "${MODE}" = "local" ] || [ "${MODE}" = "hybrid" ]; then
  echo "Ollama: http://${OLLAMA_HOST}:${OLLAMA_PORT}"
fi
echo "Ready. Use 'cc' to run Claude Code through the active mode."
