#!/usr/bin/env bash

set -euo pipefail

STATE_DIR="${HOME}/.claude/litellm"
LITELLM_PID_FILE="${STATE_DIR}/litellm.pid"
LLAMACPP_PID_FILE="${STATE_DIR}/llamacpp.pid"
LLAMACPP_PORT_FILE="${STATE_DIR}/llamacpp.port"
OLLAMA_PID_FILE="${STATE_DIR}/ollama.pid"
LITELLM_PORT="${LITELLM_PORT:-4000}"

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

  echo "Leaving port ${port} alone because it belongs to another process: ${cmd}" >&2
  return 0
}

stop_pid_file "${LITELLM_PID_FILE}" "litellm" "LiteLLM"
stop_port_if_matches "${LITELLM_PORT}" "litellm" "LiteLLM"

stop_pid_file "${LLAMACPP_PID_FILE}" "llama-server" "llama-server"
if [ -f "${LLAMACPP_PORT_FILE}" ]; then
  LLAMACPP_PORT="$(cat "${LLAMACPP_PORT_FILE}" 2>/dev/null || true)"
  if [ -n "${LLAMACPP_PORT}" ]; then
    stop_port_if_matches "${LLAMACPP_PORT}" "llama-server" "llama-server"
  fi
  rm -f "${LLAMACPP_PORT_FILE}"
fi

stop_pid_file "${OLLAMA_PID_FILE}" "ollama serve" "Ollama"

echo "Stopped owned Claude/LiteLLM helper processes."
