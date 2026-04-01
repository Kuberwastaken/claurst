#!/usr/bin/env python3

from __future__ import annotations

import os
import subprocess
from pathlib import Path
from typing import Literal

import httpx
from mcp.server.fastmcp import FastMCP


STATE_DIR = Path(
    os.environ.get("CLAUDE_LITELLM_HOME", str(Path.home() / ".claude" / "litellm"))
)
CURRENT_FILE = STATE_DIR / "current"
START_SH = STATE_DIR / "start.sh"
STATUS_SH = STATE_DIR / "status.sh"
SWITCH_BIN = Path.home() / ".local" / "bin" / "claude-switch"
ENV_FILES = [STATE_DIR / "env", STATE_DIR / "env.local"]

MOONSHOT_DEFAULT_BASE = "https://api.moonshot.ai/anthropic"
ANTHROPIC_DEFAULT_BASE = "https://api.anthropic.com"
HTTP_TIMEOUT = httpx.Timeout(600.0, connect=10.0)

MODE_CHOICES = ("local", "llamacpp", "hybrid", "cloud", "moonshot", "anthropic")
TIER_CHOICES = ("haiku", "sonnet", "opus")
TIER_MODEL_MAP = {
    "haiku": "claude-haiku-4-5-20251001",
    "sonnet": "claude-sonnet-4-6",
    "opus": "claude-opus-4-6",
}

mcp = FastMCP("model-router")


def _load_env_files() -> None:
    for env_file in ENV_FILES:
        if not env_file.exists():
            continue

        for raw_line in env_file.read_text(encoding="utf-8").splitlines():
            line = raw_line.strip()
            if not line or line.startswith("#"):
                continue
            if line.startswith("export "):
                line = line[len("export ") :].strip()
            if "=" not in line:
                continue

            key, raw_value = line.split("=", 1)
            key = key.strip()
            raw_value = raw_value.strip()
            if not key:
                continue

            if (
                len(raw_value) >= 2
                and raw_value[0] == raw_value[-1]
                and raw_value[0] in {"'", '"'}
            ):
                value = raw_value[1:-1]
            else:
                value = raw_value

            os.environ[key] = os.path.expanduser(os.path.expandvars(value))


def _run(cmd: list[str]) -> subprocess.CompletedProcess[str]:
    env = os.environ.copy()
    env.setdefault("CLAUDE_LITELLM_HOME", str(STATE_DIR))
    return subprocess.run(
        cmd,
        check=False,
        capture_output=True,
        text=True,
        timeout=900,
        env=env,
    )


def _http_ok(url: str) -> bool:
    try:
        with httpx.Client(timeout=httpx.Timeout(5.0, connect=2.0)) as client:
            response = client.get(url, follow_redirects=True)
            return response.status_code < 400
    except Exception:
        return False


def _litellm_ready() -> bool:
    return _http_ok("http://127.0.0.1:4000/health/liveliness")


def _command_output(cmd: list[str]) -> str:
    proc = _run(cmd)
    output = (proc.stdout or "").strip()
    err = (proc.stderr or "").strip()
    if proc.returncode == 0:
        return output or err or "OK"
    raise RuntimeError(
        f"Command failed ({proc.returncode}): {' '.join(cmd)}\n"
        f"{output}\n{err}".strip()
    )


def _current_mode() -> str:
    if CURRENT_FILE.exists():
        mode = CURRENT_FILE.read_text(encoding="utf-8").strip()
        if mode:
            return mode
    return "local"


def _ensure_mode(mode: str) -> str:
    if mode not in MODE_CHOICES:
        raise ValueError(f"Unsupported mode: {mode}")

    if mode in {"local", "llamacpp", "hybrid", "cloud"}:
        if _current_mode() == mode and _litellm_ready():
            return mode
        try:
            _command_output([str(START_SH), mode])
        except RuntimeError:
            if _current_mode() == mode and _litellm_ready():
                return mode
            raise
        return mode

    if _current_mode() == mode:
        return mode
    _command_output([str(SWITCH_BIN), mode])
    return mode


def _extract_text(payload: dict) -> str:
    parts = []
    for item in payload.get("content", []) or []:
        if isinstance(item, dict) and item.get("type") == "text":
            text = item.get("text", "")
            if text:
                parts.append(text)
    return "\n".join(parts).strip()


def _anthropic_messages_call(
    *,
    base_url: str,
    model: str,
    prompt: str,
    system: str | None,
    max_tokens: int,
    headers: dict[str, str],
    temperature: float | None = None,
) -> dict:
    body: dict[str, object] = {
        "model": model,
        "max_tokens": max_tokens,
        "messages": [{"role": "user", "content": [{"type": "text", "text": prompt}]}],
    }
    if system:
        body["system"] = system
    if temperature is not None:
        body["temperature"] = temperature

    with httpx.Client(timeout=HTTP_TIMEOUT, follow_redirects=True) as client:
        response = client.post(
            base_url.rstrip("/") + "/v1/messages",
            headers=headers,
            json=body,
        )
        response.raise_for_status()
        return response.json()


def _call_current_litellm(
    *, tier: str, prompt: str, system: str | None, max_tokens: int, temperature: float | None
) -> dict:
    headers = {
        "content-type": "application/json",
        "x-api-key": "mcp-local",
        "anthropic-version": "2023-06-01",
    }
    return _anthropic_messages_call(
        base_url="http://127.0.0.1:4000",
        model=TIER_MODEL_MAP[tier],
        prompt=prompt,
        system=system,
        max_tokens=max_tokens,
        headers=headers,
        temperature=temperature,
    )


def _call_moonshot(
    *, tier: str, prompt: str, system: str | None, max_tokens: int, temperature: float | None
) -> dict:
    _load_env_files()
    api_key = os.environ.get("MOONSHOT_API_KEY") or os.environ.get("KIMI_API_KEY")
    if not api_key:
        raise RuntimeError(
            "moonshot mode needs MOONSHOT_API_KEY (or KIMI_API_KEY) in ~/.claude/litellm/env"
        )

    default_model = os.environ.get("MOONSHOT_MODEL", "kimi-k2.5")
    model = {
        "haiku": os.environ.get("MOONSHOT_HAIKU_MODEL", default_model),
        "sonnet": os.environ.get("MOONSHOT_SONNET_MODEL", default_model),
        "opus": os.environ.get("MOONSHOT_OPUS_MODEL", default_model),
    }[tier]
    base_url = os.environ.get("MOONSHOT_BASE_URL", MOONSHOT_DEFAULT_BASE)

    headers = {
        "content-type": "application/json",
        "anthropic-version": "2023-06-01",
        "x-api-key": api_key,
        "authorization": f"Bearer {api_key}",
    }
    return _anthropic_messages_call(
        base_url=base_url,
        model=model,
        prompt=prompt,
        system=system,
        max_tokens=max_tokens,
        headers=headers,
        temperature=temperature,
    )


def _call_anthropic_direct(
    *, tier: str, prompt: str, system: str | None, max_tokens: int, temperature: float | None
) -> dict:
    _load_env_files()
    api_key = os.environ.get("ANTHROPIC_API_KEY")
    auth_token = os.environ.get("ANTHROPIC_AUTH_TOKEN")
    if not api_key and not auth_token:
        raise RuntimeError(
            "anthropic direct mode needs ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN in the environment"
        )

    headers = {
        "content-type": "application/json",
        "anthropic-version": "2023-06-01",
    }
    if api_key:
        headers["x-api-key"] = api_key
    if auth_token:
        headers["authorization"] = f"Bearer {auth_token}"

    base_url = os.environ.get("ANTHROPIC_BASE_URL", ANTHROPIC_DEFAULT_BASE)
    return _anthropic_messages_call(
        base_url=base_url,
        model=TIER_MODEL_MAP[tier],
        prompt=prompt,
        system=system,
        max_tokens=max_tokens,
        headers=headers,
        temperature=temperature,
    )


def _resolve_mode(mode: str) -> str:
    active_mode = _current_mode() if mode == "current" else mode
    if active_mode not in MODE_CHOICES:
        raise ValueError(f"Unsupported mode: {active_mode}")
    return active_mode


@mcp.tool(
    description="Show the current Claude/LLM backend status, including active mode and health."
)
def backend_status() -> str:
    return _command_output([str(STATUS_SH)])


@mcp.tool(
    description="List the supported backend modes and what each one routes to."
)
def list_backend_modes() -> dict[str, dict[str, str]]:
    return {
        "local": {
            "purpose": "Full local via Ollama",
            "haiku": "ollama/hermes3:8b",
            "sonnet": "ollama/qwen3-coder:30b",
            "opus": "ollama/qwen3-coder:30b-128k",
        },
        "llamacpp": {
            "purpose": "Direct llama.cpp using the local Qwen GGUF",
            "haiku": "llama.cpp qwen3-coder-30b",
            "sonnet": "llama.cpp qwen3-coder-30b",
            "opus": "llama.cpp qwen3-coder-30b",
        },
        "hybrid": {
            "purpose": "Local first, cheap cloud fallback",
            "haiku": "ollama/hermes3:8b -> deepseek/deepseek-chat",
            "sonnet": "ollama/qwen3-coder:30b -> deepseek/deepseek-chat",
            "opus": "deepseek/deepseek-chat -> together qwen3",
        },
        "cloud": {
            "purpose": "Cheap cloud-only routing through LiteLLM",
            "haiku": "deepseek/deepseek-chat",
            "sonnet": "deepseek/deepseek-chat",
            "opus": "deepseek/deepseek-reasoner",
        },
        "moonshot": {
            "purpose": "Direct Kimi via Moonshot's Anthropic-compatible endpoint",
            "haiku": "kimi-k2.5 (default)",
            "sonnet": "kimi-k2.5 (default)",
            "opus": "kimi-k2.5 or kimi-k2-thinking",
        },
        "anthropic": {
            "purpose": "Direct Anthropic backend",
            "haiku": "claude-haiku-4-5-20251001",
            "sonnet": "claude-sonnet-4-6",
            "opus": "claude-opus-4-6",
        },
    }


@mcp.tool(
    description="Switch the shared backend mode used by Claude Code and the local model router."
)
def switch_backend(
    mode: Literal["local", "llamacpp", "hybrid", "cloud", "moonshot", "anthropic"]
) -> str:
    return _command_output([str(SWITCH_BIN), mode])


@mcp.tool(
    description="Send a prompt through the shared backend stack. For local/cloud/llamacpp/hybrid this uses LiteLLM on localhost. For moonshot it uses Moonshot directly."
)
def ask_backend_model(
    prompt: str,
    tier: Literal["haiku", "sonnet", "opus"] = "sonnet",
    mode: str = "current",
    system: str = "",
    max_tokens: int = 1200,
    temperature: float | None = None,
) -> dict[str, object]:
    if tier not in TIER_CHOICES:
        raise ValueError(f"Unsupported tier: {tier}")

    active_mode = _resolve_mode(mode)
    if mode != "current":
        active_mode = _ensure_mode(active_mode)

    if active_mode in {"local", "llamacpp", "hybrid", "cloud"}:
      _ensure_mode(active_mode)
      payload = _call_current_litellm(
          tier=tier,
          prompt=prompt,
          system=system or None,
          max_tokens=max_tokens,
          temperature=temperature,
      )
      model = TIER_MODEL_MAP[tier]
    elif active_mode == "moonshot":
        payload = _call_moonshot(
            tier=tier,
            prompt=prompt,
            system=system or None,
            max_tokens=max_tokens,
            temperature=temperature,
        )
        default_model = os.environ.get("MOONSHOT_MODEL", "kimi-k2.5")
        model = {
            "haiku": os.environ.get("MOONSHOT_HAIKU_MODEL", default_model),
            "sonnet": os.environ.get("MOONSHOT_SONNET_MODEL", default_model),
            "opus": os.environ.get("MOONSHOT_OPUS_MODEL", default_model),
        }[tier]
    elif active_mode == "anthropic":
        payload = _call_anthropic_direct(
            tier=tier,
            prompt=prompt,
            system=system or None,
            max_tokens=max_tokens,
            temperature=temperature,
        )
        model = TIER_MODEL_MAP[tier]
    else:
        raise ValueError(f"Unsupported mode: {active_mode}")

    text = _extract_text(payload)
    return {
        "mode": active_mode,
        "tier": tier,
        "model": model,
        "text": text,
        "stop_reason": payload.get("stop_reason"),
        "usage": payload.get("usage"),
    }


@mcp.tool(
    description="Run a tiny smoke test against the chosen backend and return the response payload."
)
def smoke_test_backend(
    mode: str = "current",
    tier: Literal["haiku", "sonnet", "opus"] = "sonnet",
) -> dict[str, object]:
    return ask_backend_model(
        prompt="Reply with exactly OK and nothing else.",
        tier=tier,
        mode=mode,
        max_tokens=16,
        temperature=0,
    )


if __name__ == "__main__":
    mcp.run()
