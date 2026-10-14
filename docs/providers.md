# LLM Providers

Claurst supports a wide range of LLM providers through a unified provider abstraction. Every provider implements the same `LlmProvider` trait, so switching between them requires only a configuration change.

> Running a model on your own machine (llama.cpp, LM Studio, Ollama, vLLM)? See
> the dedicated [Local Models](local-models) guide for recommended server flags,
> tool-calling setup, model guidance, and cache accounting.

---

## Selecting a Provider

Use the `--provider` flag on any invocation to override the active provider:

```
claurst --provider openai "refactor this module"
claurst --provider ollama "explain this function"
claurst --provider groq --model llama-3.3-70b-versatile "write tests"
```

The provider can also be set persistently in `~/.claurst/settings.json`:

```json
{
  "provider": "openai"
}
```

When no provider is specified, Claurst defaults to **Anthropic**, unless the model id is written as `provider/model` (for example `--model openai/gpt-4o`), which selects that provider.

**Default models:** when a provider is selected but no model is set, Claurst picks the highest-priority model for that provider from the model catalog (see [Model Registry](#model-registry)). The **Default model** values below are the fallbacks used when the catalog has no entry for the provider.

---

## Provider Reference

### Anthropic (default)

The default provider. Uses the `/v1/messages` streaming endpoint.

**Authentication:** `ANTHROPIC_API_KEY` environment variable, `api_key` in `settings.json`, or OAuth login (`claurst auth login`). See [Authentication](auth).

**Default model:** `claude-opus-4-6`

**Selected models (bundled snapshot; the catalog lists other Claude models too):**

| Model ID | Context Window | Max Output | Input ($/1M) | Output ($/1M) |
|---|---|---|---|---|
| `claude-opus-4-6` | 1,000,000 | 128,000 | $5.00 | $25.00 |
| `claude-sonnet-4-6` | 1,000,000 | 64,000 | $3.00 | $15.00 |
| `claude-haiku-4-5-20251001` | 200,000 | 64,000 | $1.00 | $5.00 |

These models support tool calling, vision, and extended reasoning. The `/model` picker intersects the catalog with the models `GET /v1/models` returns for the active credential.

**Configuration:**

```json
{
  "provider": "anthropic",
  "providers": {
    "anthropic": {
      "api_key": "sk-ant-...",
      "models_whitelist": ["claude-sonnet-4-6", "claude-haiku-4-5-20251001"]
    }
  }
}
```

**Base URL override:** Set `ANTHROPIC_BASE_URL` to point at a proxy or local mirror.

---

### OpenAI

Uses the OpenAI Chat Completions API (`/v1/chat/completions`); model ids starting with `o3`, `o4`, or `gpt-5` use the Responses API instead.

**Authentication:** `OPENAI_API_KEY` environment variable.

**Base URL override:** `OPENAI_BASE_URL` or `api_base`; a trailing `/v1` is accepted.

**Default model:** `gpt-4o`

**Available models (bundled snapshot):**

| Model ID | Context Window | Max Output | Reasoning |
|---|---|---|---|
| `gpt-4o` | 128,000 | 16,384 | No |
| `gpt-4o-mini` | 128,000 | 16,384 | No |
| `o3` | 200,000 | 100,000 | Yes |
| `o4-mini` | 200,000 | 100,000 | Yes |

**Configuration:**

```json
{
  "provider": "openai",
  "providers": {
    "openai": {
      "api_key": "sk-...",
      "api_base": "https://api.openai.com/v1"
    }
  }
}
```

---

### Google (Gemini)

Uses the Google Generative Language API (`generativelanguage.googleapis.com`). Vertex AI is not supported.

**Authentication:** `GOOGLE_API_KEY` or `GOOGLE_GENERATIVE_AI_API_KEY` environment variable.

**Default model:** `gemini-2.5-flash`

**Available models (bundled snapshot):**

| Model ID | Context Window | Max Output |
|---|---|---|
| `gemini-2.5-pro` | 1,048,576 | 65,536 |
| `gemini-2.5-flash` | 1,048,576 | 65,536 |
| `gemini-2.0-flash` | 1,048,576 | 8,192 |

**Configuration:**

```json
{
  "provider": "google",
  "providers": {
    "google": {
      "api_key": "AIza..."
    }
  }
}
```

---

### Azure OpenAI

Uses the Azure OpenAI Chat Completions endpoint. The deployment name acts as the model identifier.

**Authentication:**
- `AZURE_API_KEY` — your Azure OpenAI API key (or `api_key` in settings)
- `AZURE_RESOURCE_NAME` — your Azure resource name (the subdomain of `.openai.azure.com`), or `options.resource_name` in settings
- `AZURE_API_VERSION` — optional API version, read only when the provider is built from the environment; defaults to `2024-08-01-preview`

**Default model:** `gpt-4o`

**Request URL format:**

```
https://{AZURE_RESOURCE_NAME}.openai.azure.com/openai/deployments/{deployment}/chat/completions?api-version={version}
```

**Configuration:**

```json
{
  "provider": "azure",
  "providers": {
    "azure": {
      "api_key": "...",
      "options": {
        "resource_name": "my-azure-resource"
      }
    }
  }
}
```

---

### AWS Bedrock

Uses the Bedrock Converse Streaming API. Supports all Claude models deployed on Bedrock.

**Authentication (two modes):**

1. **Bearer token:** Set `AWS_BEARER_TOKEN_BEDROCK` (takes priority over SigV4).
2. **SigV4 credentials:** Set `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and optionally `AWS_SESSION_TOKEN`.

**Region:** Reads `AWS_REGION` or `AWS_DEFAULT_REGION` (defaults to `us-east-1`).

**Default model:** `anthropic.claude-sonnet-4-6-v1`

For model ids that contain no `.`, the adapter adds a cross-region inference prefix: `us.` for Claude and Nova models in non-GovCloud `us-*` regions, `eu.` for Claude models in `eu-*` regions. Ids that already contain a `.` (such as `anthropic.claude-...`) are sent unchanged.

**Configuration:** Bedrock is configured only through the environment variables above; `providers.amazon-bedrock` settings are not read. Select it with:

```json
{
  "provider": "amazon-bedrock"
}
```

---

### GitHub Copilot

Uses the GitHub Copilot Chat Completions API (`https://api.githubcopilot.com/chat/completions`).

**Authentication:** `GITHUB_TOKEN` environment variable, `api_key` in settings, or the GitHub device-code login in `/connect` (stored in `~/.claurst/auth.json`).

**Default model:** `gpt-4o`

**Configuration:**

```json
{
  "provider": "github-copilot",
  "providers": {
    "github-copilot": {
      "api_key": "ghu_..."
    }
  }
}
```

---

### OpenAI Codex (ChatGPT subscription)

Uses a ChatGPT subscription login against the Codex Responses endpoint (`https://chatgpt.com/backend-api/codex/responses`). Provider id `codex` (alias `openai-codex`).

**Authentication:** OAuth only. Run `claurst codex login` or choose OpenAI Codex in `/connect`. Tokens are stored per profile under `~/.claurst/accounts/codex/` and refreshed automatically; see [Authentication](auth).

**Default model:** `gpt-5.5`. The model list is the models.dev `openai` catalog filtered to `gpt-5.5`, `gpt-5.4`, `gpt-5.4-mini`, `gpt-5.3-codex-spark`, and newer `gpt-<major>.<minor>` releases (`gpt-5.5-pro` excluded).

```
claurst --provider codex --model gpt-5.5
```

---

### Cohere

Native Cohere API adapter.

**Authentication:** `COHERE_API_KEY` environment variable.

**Default model:** `command-r-plus`

**Configuration:**

```json
{
  "provider": "cohere",
  "providers": {
    "cohere": {
      "api_key": "..."
    }
  }
}
```

---

### MiniMax

The built-in provider uses the Anthropic-compatible Messages API.

**Authentication:** `MINIMAX_API_KEY` environment variable, or set `api_key` in `settings.json`.

**Default model:** `MiniMax-M3`

| Model | Context window | Input modalities | Thinking |
|---|---:|---|---|
| `MiniMax-M3` | 1,000,000 | Text, image, video | Off by default; supports `adaptive` and `disabled` |
| `MiniMax-M2.7` | 204,800 | Text | Always on |

The catalog retains the model's complete input-modality metadata. Claurst's built-in attachment flow currently sends text and image blocks.

Pricing is in USD per million tokens:

| Model | Service tier | Input range | Input | Output | Cache read | Cache write |
|---|---|---:|---:|---:|---:|---:|
| `MiniMax-M3` | Standard | Up to 512k | $0.30 | $1.20 | $0.06 | Not published |
| `MiniMax-M3` | Standard | Over 512k | $0.60 | $2.40 | $0.12 | Not published |
| `MiniMax-M3` | Priority | Up to 512k | $0.45 | $1.80 | $0.09 | Not published |
| `MiniMax-M3` | Priority | Over 512k | $0.90 | $3.60 | $0.18 | Not published |
| `MiniMax-M2.7` | Standard | All requests | $0.30 | $1.20 | $0.06 | $0.375 |

| Protocol | Global base URL | China base URL | Path added by Claurst |
|---|---|---|---|
| Anthropic | `https://api.minimax.io/anthropic` | `https://api.minimaxi.com/anthropic` | `/v1/messages` |
| OpenAI-compatible | `https://api.minimax.io/v1` | `https://api.minimaxi.com/v1` | `/chat/completions` |

The built-in `minimax` provider uses the Anthropic row. To use the China endpoint, set `MINIMAX_BASE_URL` or configure `api_base`:

```json
{
  "provider": "minimax",
  "model": "MiniMax-M3",
  "providers": {
    "minimax": {
      "api_key": "...",
      "api_base": "https://api.minimaxi.com/anthropic"
    }
  }
}
```

MiniMax-M3 uses the standard service tier by default. To request priority admission, set `service_tier` in the provider options:

```json
{
  "provider": "minimax",
  "model": "MiniMax-M3",
  "providers": {
    "minimax": {
      "api_key": "...",
      "options": {
        "service_tier": "priority"
      }
    }
  }
}
```

For the OpenAI-compatible protocol, use the custom provider with the corresponding `/v1` base URL:

```json
{
  "provider": "custom-openai",
  "model": "MiniMax-M3",
  "providers": {
    "custom-openai": {
      "api_key": "...",
      "api_base": "https://api.minimax.io/v1"
    }
  }
}
```

---

### Ollama

Connects to a locally running Ollama instance. No API key required.

**Base URL:** Reads `OLLAMA_HOST` (defaults to `http://localhost:11434`). Claurst appends `/v1` to construct the OpenAI-compatible endpoint.

**Default model:** `llama3.2`

**Model list:** When using `/connect` or `/model`, the picker queries your local Ollama server via `/api/tags` and shows only the models you have installed (`ollama list`). Cloud models (e.g., `kimi-k2.6:cloud`) appear after you run `ollama pull <model>:cloud`.

**Configuration:**

```json
{
  "provider": "ollama",
  "providers": {
    "ollama": {
      "api_base": "http://localhost:11434"
    }
  }
}
```

Run a model locally first with `ollama pull llama3.2`, then:

```
claurst --provider ollama --model llama3.2 "explain this code"
```

---

### LM Studio (local)

Connects to a locally running LM Studio server. No API key required.

**Base URL:** Reads `LM_STUDIO_HOST` (defaults to `http://localhost:1234`). Claurst appends `/v1`.

**Default model:** `default` (whichever model is loaded in LM Studio)

Provider id `lmstudio` (alias `lm-studio`).

**Configuration:**

```json
{
  "provider": "lmstudio",
  "providers": {
    "lmstudio": {
      "api_base": "http://localhost:1234/v1"
    }
  }
}
```

---

### LLaMA.cpp (local)

Connects to a locally running llama.cpp HTTP server. No API key required.

**Base URL:** Reads `LLAMA_CPP_HOST` (defaults to `http://localhost:8080`). Claurst appends `/v1`.

**Default model:** `default`

Provider id `llamacpp` (aliases `llama-cpp`, `llama-server`).

**Configuration:**

```json
{
  "provider": "llamacpp",
  "providers": {
    "llamacpp": {
      "api_base": "http://localhost:8080/v1"
    }
  }
}
```

Start the llama.cpp server (`llama-server`) before use.

For recommended `llama-server` flags (tool calling, context sizing, prompt
caching), model guidance, and cache-accounting details, see the
[Local Models](local-models) guide.

---

### Groq

Fast inference cloud with OpenAI-compatible API.

**Authentication:** `GROQ_API_KEY` environment variable.

**Base URL:** `https://api.groq.com/openai/v1`

**Default model:** `llama-3.3-70b-versatile`

**Configuration:**

```json
{
  "provider": "groq",
  "providers": {
    "groq": {
      "api_key": "gsk_..."
    }
  }
}
```

---

### DeepSeek

OpenAI-compatible API with extended reasoning output via a `reasoning_content` field.

**Authentication:** `DEEPSEEK_API_KEY` environment variable.

**Base URL:** `https://api.deepseek.com/v1`

**Default model:** `deepseek-v4-pro`

**Configuration:**

```json
{
  "provider": "deepseek",
  "providers": {
    "deepseek": {
      "api_key": "sk-..."
    }
  }
}
```

---

### Mistral AI

OpenAI-compatible API with Mistral-specific protocol quirks (tool call ID formatting, tool-user sequence injection).

**Authentication:** `MISTRAL_API_KEY` environment variable.

**Base URL:** `https://api.mistral.ai/v1`

**Default model:** `mistral-large-latest`

**Configuration:**

```json
{
  "provider": "mistral",
  "providers": {
    "mistral": {
      "api_key": "..."
    }
  }
}
```

---

### xAI (Grok)

**Authentication:** `XAI_API_KEY` environment variable.

**Base URL:** `https://api.x.ai/v1`

**Default model:** `grok-2`

**Configuration:**

```json
{
  "provider": "xai",
  "providers": {
    "xai": {
      "api_key": "xai-..."
    }
  }
}
```

---

### OpenRouter

Unified API gateway to many models. Sends `HTTP-Referer: https://claurst.ai/` and `X-Title: Claurst` headers automatically.

**Authentication:** `OPENROUTER_API_KEY` environment variable.

**Base URL:** `https://openrouter.ai/api/v1`

**Default model:** `anthropic/claude-sonnet-4`

Model identifiers use OpenRouter's routing format: `provider/model-name`.

**Configuration:**

```json
{
  "provider": "openrouter",
  "providers": {
    "openrouter": {
      "api_key": "sk-or-..."
    }
  }
}
```

---

### Together AI

Hosted open-source models. Provider id `together-ai`; `togetherai` is not accepted by `--provider`.

**Authentication:** `TOGETHER_API_KEY` environment variable.

**Base URL:** `https://api.together.xyz/v1`

**Default model:** `meta-llama/Llama-3.3-70B-Instruct-Turbo`

**Configuration:**

```json
{
  "provider": "together-ai",
  "providers": {
    "together-ai": {
      "api_key": "..."
    }
  }
}
```

---

### Perplexity

Search-augmented LLM API.

**Authentication:** `PERPLEXITY_API_KEY` environment variable.

**Base URL:** `https://api.perplexity.ai`

**Default model:** `sonar-pro`

**Configuration:**

```json
{
  "provider": "perplexity",
  "providers": {
    "perplexity": {
      "api_key": "pplx-..."
    }
  }
}
```

---

### DeepInfra

Hosted open-weight models on OpenAI-compatible API.

**Authentication:** `DEEPINFRA_API_KEY` environment variable.

**Base URL:** `https://api.deepinfra.com/v1/openai`

**Default model:** `meta-llama/Llama-3.3-70B-Instruct`

**Configuration:**

```json
{
  "provider": "deepinfra",
  "providers": {
    "deepinfra": {
      "api_key": "..."
    }
  }
}
```

---

### Venice AI

Privacy-focused inference.

**Authentication:** `VENICE_API_KEY` environment variable.

**Base URL:** `https://api.venice.ai/api/v1`

**Default model:** `llama-3.3-70b` (resolved from the model registry at runtime)

**Configuration:**

```json
{
  "provider": "venice",
  "providers": {
    "venice": {
      "api_key": "..."
    }
  }
}
```

---

### Cerebras

Wafer-scale inference hardware.

**Authentication:** `CEREBRAS_API_KEY` environment variable.

**Base URL:** `https://api.cerebras.ai/v1`

**Default model:** `llama-3.3-70b`

**Configuration:**

```json
{
  "provider": "cerebras",
  "providers": {
    "cerebras": {
      "api_key": "..."
    }
  }
}
```

---

### Other OpenAI-compatible providers

These ids are also accepted by `--provider`. Each reads the listed environment variable, `api_key` in settings, or a key stored by `/connect`.

| Provider id | Environment variable | Base URL |
|---|---|---|
| `sambanova` | `SAMBANOVA_API_KEY` | `https://api.sambanova.ai/v1` |
| `huggingface` | `HF_TOKEN` | `https://router.huggingface.co/v1` |
| `nvidia` | `NVIDIA_API_KEY` | `https://integrate.api.nvidia.com/v1` |
| `qwen` (alias `alibaba`) | `DASHSCOPE_API_KEY` | `https://dashscope.aliyuncs.com/compatible-mode/v1` |
| `siliconflow` | `SILICONFLOW_API_KEY` | `https://api.siliconflow.com/v1` |
| `moonshotai` (alias `moonshot`) | `MOONSHOT_API_KEY` | `https://api.moonshot.ai/v1` |
| `zhipuai` (alias `zhipu`) | `ZHIPU_API_KEY` | `https://open.bigmodel.cn/api/paas/v4` |
| `zai` | `ZAI_API_KEY` | `https://api.z.ai/api/coding/paas/v4` |
| `nebius` | `NEBIUS_API_KEY` | `https://api.tokenfactory.nebius.com/v1` |
| `novita` | `NOVITA_API_KEY` | `https://api.novita.ai/v3/openai` |
| `ovhcloud` | `OVHCLOUD_API_KEY` | `https://oai.endpoints.kepler.ai.cloud.ovh.net/v1` |
| `scaleway` | `SCALEWAY_API_KEY` | `https://api.scaleway.ai/v1` |
| `vultr` | `VULTR_API_KEY` | `https://api.vultrinference.com/v1` |
| `baseten` | `BASETEN_API_KEY` | `https://inference.baseten.co/v1` |
| `friendli` | `FRIENDLI_TOKEN` | `https://api.friendli.ai/serverless/v1` |
| `upstage` | `UPSTAGE_API_KEY` | `https://api.upstage.ai/v1/solar` |
| `stepfun` | `STEPFUN_API_KEY` | `https://api.stepfun.com/v1` |
| `fireworks` | `FIREWORKS_API_KEY` | `https://api.fireworks.ai/inference/v1` |

`custom-openai` is a generic OpenAI-compatible provider: configure it in `/connect` (Custom OpenAI-Compatible: URL + API key), or set `providers.custom-openai.api_base` (default `http://localhost:11434/v1`) and `api_key` in settings. It is only registered when an API key is configured.

---

## Per-Provider Configuration in settings.json

The `providers` map in `~/.claurst/settings.json` accepts per-provider `ProviderConfig` objects:

```json
{
  "provider": "anthropic",
  "providers": {
    "anthropic": {
      "api_key": "sk-ant-...",
      "api_base": "https://api.anthropic.com",
      "enabled": true,
      "models_whitelist": [],
      "models_blacklist": [],
      "options": {}
    },
    "openai": {
      "api_key": "sk-...",
      "enabled": true
    },
    "ollama": {
      "enabled": true,
      "api_base": "http://192.168.1.50:11434/v1"
    }
  }
}
```

**Fields:**

| Field | Type | Description |
|---|---|---|
| `api_key` | string | Takes precedence over the environment variable API key |
| `api_base` | string | Override the default base URL. Applied for `anthropic`, `openai`, `minimax`, `ollama`, `lmstudio`, `llamacpp`, `deepseek`, `groq`, `xai`, `openrouter`, and `custom-openai`; other providers use their built-in URL. |
| `enabled` | bool | Enable or disable the provider (default: `true`) |
| `models_whitelist` | array of strings | Parsed but not currently enforced |
| `models_blacklist` | array of strings | Parsed but not currently enforced |
| `options` | object | Provider-specific options. Keys read today: `resource_name` (azure), `service_tier` (minimax). |
| `requestTimeoutSecs` | integer | Per-provider request timeout override (alias `request_timeout_secs`) |

## Model Registry

Claurst embeds a snapshot of the `https://models.dev/api.json` catalog (about 118 providers). At runtime it refreshes the catalog in the background into the OS cache directory (`<cache dir>/claurst/models.json`, for example `~/.cache/claurst/models.json` on Linux or `~/Library/Caches/claurst/models.json` on macOS), at most once every 5 minutes, and hourly during a long TUI session. Network failures are ignored; the embedded snapshot is always available.

- `CLAURST_DISABLE_MODELS_FETCH` disables the network refresh.
- `CLAURST_MODELS_URL` (or `MODELS_DEV_URL`) points the refresh at another URL.
- `CLAURST_MODELS_PATH` loads the catalog from a local file instead of the cache.
- `claurst models [provider] [--refresh] [--verbose] [--json]` lists the catalog.

When no model is explicitly set, Claurst scores available models by priority patterns to pick the best default. Well-known model prefixes (`claude-*`, `gpt-*`, `gemini-*`, etc.) are always routed to their canonical provider regardless of gateway entries in the remote cache.

### Overriding model metadata

Self-hosted endpoints (the `custom-openai` / Ollama / LM Studio / llama.cpp
providers) and model aliases that models.dev does not know can end up with the
wrong context window or max-output size — either because the alias is matched to
an unrelated catalog entry, or because there is no catalog entry at all. The
`modelOverrides` map lets you supply or correct that metadata. **User overrides
take precedence over the models.dev catalog and over the built-in defaults.**

Add it at the top level of `~/.claurst/settings.json` (or inside the `config`
object), keyed by the fully-qualified `"provider/model"` id:

```json
{
  "modelOverrides": {
    "custom-openai/my-local-llm": {
      "contextWindow": 32768,
      "maxOutputTokens": 4096,
      "name": "My Local LLM",
      "releaseDate": "2026-01-01",
      "status": "beta"
    },
    "ollama/qwen3-coder-30b": {
      "contextWindow": 262144
    }
  }
}
```

**Fields** (all optional — an unset field keeps the catalog value):

| Field | Type | Description |
|---|---|---|
| `contextWindow` | integer | Total context window size in tokens |
| `maxOutputTokens` | integer | Maximum tokens the model can emit in one response |
| `name` | string | Human-readable display name shown in the model picker |
| `releaseDate` | string | ISO 8601 date; drives newest-first ordering in the picker |
| `status` | string | Lifecycle status (`active`, `beta`, `alpha`, `deprecated`) |

Field names accept both camelCase (`contextWindow`) and snake_case
(`context_window`). The key **must** contain a `/` — a bare model id is ignored,
because the registry is keyed by `provider/model`.

When the keyed model exists in the catalog, the override patches it in place.
When it does not (a self-hosted alias), Claurst materialises a synthetic entry
so the corrected values flow everywhere the metadata is read: the `/model`
picker, the token-usage warnings, and the auto-compact thresholds.
