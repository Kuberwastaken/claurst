# Claurst Authentication Guide

Claurst needs credentials to call the Anthropic API (or another provider's
API). This document covers every supported authentication method, multi-account
profile switching, how tokens are stored, how to check and clear credentials,
and how to authenticate with non-Anthropic providers.

---

## Authentication Methods

Claurst checks for credentials in the following priority order:

1. `--api-key` flag (highest priority, session-only)
2. `api_key` field in `~/.claurst/settings.json`
3. `ANTHROPIC_API_KEY` environment variable
4. Tokens for the **active Anthropic profile** under
   `~/.claurst/accounts/anthropic/<id>/oauth_tokens.json`
5. Legacy `~/.claurst/oauth_tokens.json` (auto-migrated to a profile on first
   read)

The first non-empty credential found is used. Provider-specific credentials
(OpenAI, Google, etc.) follow the same pattern but use their own environment
variables and provider config entries.

Codex (OpenAI ChatGPT subscription) accounts follow a parallel system —
multiple profiles stored under `~/.claurst/accounts/codex/<id>/`, with the
active profile selected via the account registry.

---

## Method 1: API Key

The simplest and most reliable authentication method is a direct API key from
the Anthropic Console.

### Get an API key

1. Log in to [console.anthropic.com](https://console.anthropic.com).
2. Navigate to **Settings > API Keys**.
3. Click **Create Key** and copy the generated `sk-ant-...` key.

### Configure the key

**Option A: Environment variable (recommended)**

Set `ANTHROPIC_API_KEY` in your shell profile. This keeps the key out of any
configuration files that might be committed to version control.

```bash
# Add to ~/.bashrc or ~/.zshrc
export ANTHROPIC_API_KEY="sk-ant-api03-..."
```

On Windows (Command Prompt, permanent):

```cmd
setx ANTHROPIC_API_KEY "sk-ant-api03-..."
```

On Windows (PowerShell profile):

```powershell
$env:ANTHROPIC_API_KEY = "sk-ant-api03-..."
# To persist it:
[System.Environment]::SetEnvironmentVariable("ANTHROPIC_API_KEY","sk-ant-api03-...","User")
```

**Option B: Settings file**

Store the key in `~/.claurst/settings.json`. Ensure the file has restricted
permissions on shared systems.

```json
{
  "config": {
    "api_key": "sk-ant-api03-..."
  }
}
```

**Option C: CLI flag (session-only)**

Pass the key directly for a single run. It is not persisted anywhere.

```bash
claurst --api-key "sk-ant-api03-..." "your prompt"
```

---

## Method 2: OAuth Login (Browser-based)

Claurst supports an OAuth 2.0 PKCE flow that authenticates through either
the Anthropic Console or Claude.ai in your browser.

> **Important:** The OAuth client IDs in Claurst are registered to Anthropic's
> official Claude Code CLI application. Anthropic's authorization server may
> reject or misattribute OAuth requests originating from Claurst. The API key
> method is the recommended path for Claurst users.
>
> If OAuth login is attempted and fails, use Method 1 (API key) instead.

### Claude.ai flow (default)

```bash
claurst auth login
```

1. Claurst generates a PKCE code verifier and code challenge.
2. A temporary localhost HTTP server starts on a random port to receive the
   callback.
3. The authorization URL is printed to the terminal and Claurst attempts to
   open it in your default browser.
4. Complete the authorization in the browser (Claude.ai login page).
5. The browser redirects to `http://localhost:<port>/callback` with an
   authorization code.
6. Claurst exchanges the code for tokens via the token endpoint.
7. Tokens are saved under `~/.claurst/accounts/anthropic/<profile-id>/oauth_tokens.json`
   and the profile is registered as **active** in `~/.claurst/accounts.json`.

This flow produces a Bearer token (`user:inference` scope) used directly for
API calls.

### Console flow (creates an API key)

```bash
claurst auth login --console
```

This uses the Anthropic Console authorization endpoint. After token exchange,
Claurst calls the Console API to create a new API key, stores it in the
active profile's `oauth_tokens.json`, and uses it as a standard API key for
subsequent requests (not as a Bearer token).

### Naming the profile

Add `--label <name>` to give the new profile a human-friendly name (otherwise
the id is derived from the JWT email's local-part). This becomes the id you
use when running `claurst auth switch`:

```bash
claurst auth login --label work
claurst auth login --label personal
claurst auth switch personal
```

### Manual fallback

If the browser does not open automatically, Claurst prints the full
authorization URL. Copy and paste it into a browser. After you authorize,
paste the authorization code shown in the browser back into the terminal
when prompted.

---

## Multi-Account Profiles

Claurst stores **multiple named accounts per provider** and lets you switch
between them without re-logging-in. Supported providers today: **Anthropic**
(Claude.ai / Console) and **Codex** (OpenAI ChatGPT subscription).

This is useful for separating work and personal accounts, juggling
Pro/Max/Team plans, or testing against multiple organizations.

### On-disk layout

```
~/.claurst/
├── accounts.json                              # registry (active + metadata)
└── accounts/
    ├── anthropic/
    │   ├── work/oauth_tokens.json
    │   └── personal/oauth_tokens.json
    └── codex/
        └── work/codex_tokens.json
```

`accounts.json` schema (excerpt):

```json
{
  "version": 1,
  "providers": {
    "anthropic": {
      "active": "personal",
      "profiles": {
        "work":     { "id": "work",     "email": "kuber@company.example",  "subscription_tier": "max", "added_at": "2026-05-25T19:00:00Z" },
        "personal": { "id": "personal", "email": "kuber@personal.example", "subscription_tier": "pro", "added_at": "2026-05-25T19:05:00Z" }
      }
    },
    "codex": {
      "active": "work",
      "profiles": { "work": { "id": "work", "email": "kuber@company.example" } }
    }
  }
}
```

### CLI

`claurst auth` and `claurst codex` are symmetric — same subcommands for both
providers:

```bash
# Add accounts (each login becomes its own profile)
claurst auth login                       # Claude.ai (default)
claurst auth login --console             # Console / API-key flow
claurst auth login --label work          # name the profile
claurst codex login                      # ChatGPT/Codex OAuth
claurst codex login --label personal

# Inspect
claurst auth status                      # show active Anthropic profile
claurst auth list                        # all Anthropic profiles
claurst codex list                       # all Codex profiles
claurst accounts                         # both at once (use --json for JSON)

# Switch the active account
claurst auth switch work
claurst codex switch personal

# Remove a stored profile
claurst auth remove work                 # delete profile + tokens dir
claurst codex remove personal

# Logout (clears tokens for the active profile)
claurst auth logout
claurst codex logout
```

`claurst auth status` and `claurst codex status` exit `0` when logged in and
`1` otherwise, so they can drive scripts:

```bash
if claurst codex status > /dev/null; then
  echo "Codex login present"
fi
```

### Slash commands

Inside the interactive REPL the same operations are available as slash
commands — Anthropic is the default, pass `--codex` to target Codex:

```
/login                          # OAuth login (Claude.ai)
/login --console                # API-key flow
/login --codex                  # add a Codex account
/login --label work             # name the new profile
/logout                         # clear active Anthropic credentials
/logout --codex                 # clear active Codex credentials
/logout --all                   # purge every stored Anthropic profile
/accounts                       # list every stored account
/switch personal                # set active Anthropic to "personal"
/switch --codex work            # set active Codex to "work"
```

`/accounts` lists every profile with a `*` next to the active one and shows
email and subscription tier when known.

### Identity detection

When you log in, Claurst decodes the JWT id_token (or access token for Codex)
to extract your email and provider-side account_id. If a stored profile
already matches that identity, the existing profile is refreshed instead of
a duplicate being created — re-logging-in the same account is idempotent.

### Backward compatibility

If you previously used Claurst (with the older single-file storage), your
existing tokens are auto-migrated on first read:

- `~/.claurst/oauth_tokens.json` → `~/.claurst/accounts/anthropic/<derived>/oauth_tokens.json`
- `~/.claurst/codex_tokens.json` → `~/.claurst/accounts/codex/<derived>/codex_tokens.json`

The legacy files are removed after a successful migration. No manual action
needed.

---

## Method 3: Device Code Flow

The device code flow (RFC 8628) is designed for headless or server
environments where opening a browser is not practical. Currently this flow
is used internally for GitHub Copilot authentication.

For headless environments without a Copilot subscription, the API key method
(Method 1) is the recommended approach. Set `ANTHROPIC_API_KEY` in the
environment before running Claurst in a CI/CD or server context.

```bash
# Headless / CI example
ANTHROPIC_API_KEY="sk-ant-..." claurst --print "summarize the last 10 commits"
```

---

## Token Storage

### Anthropic OAuth tokens (per profile)

Each Anthropic account profile has its own file:

```
~/.claurst/accounts/anthropic/<profile-id>/oauth_tokens.json
```

The file contains the access token, optional refresh token, expiry timestamp,
granted scopes, and account email. Example structure:

```json
{
  "access_token": "...",
  "refresh_token": "...",
  "expires_at_ms": 1700000000000,
  "scopes": ["user:inference", "user:profile"],
  "email": "you@example.com",
  "api_key": "sk-ant-..."
}
```

The active profile pointer lives in `~/.claurst/accounts.json` (see
[Multi-Account Profiles](#multi-account-profiles)). Files are written with
user-only permissions (`600` on Unix). Do not commit them to version control.

### Codex tokens (per profile)

```
~/.claurst/accounts/codex/<profile-id>/codex_tokens.json
```

Contains the OpenAI access token, refresh token, account_id, and expiry.

### Provider credential store

API keys for non-Anthropic providers without dedicated OAuth flows are stored in:

```
~/.claurst/auth.json
```

This file is keyed by provider ID and contains either an `api` credential
(plain key) or an `oauth` credential (access + refresh token pair):

```json
{
  "credentials": {
    "openai": { "type": "api", "key": "sk-..." },
    "github-copilot": {
      "type": "oauth",
      "access": "...",
      "refresh": "...",
      "expires": 1700000000
    }
  }
}
```

> **Note:** `~/.claurst/auth.json` is the multi-provider credential cache for
> simple API-key providers. It is **distinct** from `~/.claurst/accounts.json`,
> which is the multi-account registry for Anthropic/Codex OAuth profiles.

---

## Checking Authentication Status

```bash
claurst auth status
```

Prints a human-readable summary:

```
Logged in.
  API provider: Anthropic
  Login method: API Key
  Billing mode: API
  Key source:   ANTHROPIC_API_KEY
```

For machine-readable output:

```bash
claurst auth status --json
```

Example JSON output:

```json
{
  "loggedIn": true,
  "authMethod": "api_key",
  "apiProvider": "Anthropic",
  "billing": "API",
  "apiKeySource": "ANTHROPIC_API_KEY"
}
```

The exit code is `0` when logged in, `1` when not logged in. This makes
`auth status` suitable for scripting:

```bash
if claurst auth status > /dev/null 2>&1; then
  echo "credentials present"
fi
```

---

## Logging Out

By default, `logout` removes the **active** account's tokens and drops that
profile from the registry; other stored profiles are untouched, so a stored
secondary profile becomes the candidate for next selection.

```bash
# Remove the active Anthropic profile
claurst auth logout

# Remove the active Codex profile
claurst codex logout

# Or from inside the REPL
/logout
/logout --codex
```

To purge every stored profile for a provider (and clear any API key in
`settings.json`):

```
/logout --all          # Anthropic
/logout --codex --all  # Codex
```

API keys set via environment variables are not affected by `logout`; remove
them from your shell profile manually.

To delete a specific stored profile without making it active first:

```bash
claurst auth remove work
claurst codex remove personal
```

---

## Token Refresh

When Claurst loads OAuth tokens for the active profile and the access token
is expired, it automatically attempts a silent refresh:

1. A `POST` request is sent to the provider's token endpoint with the stored
   refresh token.
2. If successful, the new access token (and optionally a new refresh token)
   is written back to the same per-profile token file.
3. The refreshed token is used for the current session.

If the refresh fails (network error, expired refresh token, revoked grant),
Claurst falls back to any configured API key. If no API key is available,
authentication fails and you must run `claurst auth login` (optionally with
`--label <name>` to reuse a profile id) again.

---

## Multiple Providers

Claurst supports simultaneous configuration of credentials for multiple
providers. Each provider looks for credentials in this order:

1. `api_key` in the provider's entry under `providers` in `settings.json`
2. The provider-specific environment variable (see table below)
3. The credential stored in `~/.claurst/auth.json`

### Provider environment variables

| Provider | Environment variable |
|----------|---------------------|
| `anthropic` | `ANTHROPIC_API_KEY` |
| `openai` | `OPENAI_API_KEY` |
| `google` | `GOOGLE_API_KEY` |
| `groq` | `GROQ_API_KEY` |
| `cerebras` | `CEREBRAS_API_KEY` |
| `deepseek` | `DEEPSEEK_API_KEY` |
| `mistral` | `MISTRAL_API_KEY` |
| `xai` | `XAI_API_KEY` |
| `openrouter` | `OPENROUTER_API_KEY` |
| `togetherai` | `TOGETHER_API_KEY` |
| `perplexity` | `PERPLEXITY_API_KEY` |
| `cohere` | `COHERE_API_KEY` |
| `deepinfra` | `DEEPINFRA_API_KEY` |
| `venice` | `VENICE_API_KEY` |
| `github-copilot` | `GITHUB_TOKEN` |
| `azure` | `AZURE_API_KEY` |
| `huggingface` | `HF_TOKEN` |
| `nvidia` | `NVIDIA_API_KEY` |

### Example: multiple providers in settings.json

```json
{
  "providers": {
    "anthropic": {
      "api_key": null,
      "enabled": true
    },
    "openai": {
      "api_key": "sk-...",
      "enabled": true
    },
    "ollama": {
      "api_base": "http://localhost:11434",
      "enabled": true
    },
    "openrouter": {
      "api_key": "sk-or-...",
      "enabled": true,
      "models_whitelist": ["anthropic/claude-sonnet-4", "openai/gpt-4o"]
    }
  }
}
```

Switch providers at runtime:

```bash
# Use OpenAI for this session
claurst --provider openai --model gpt-4o "your prompt"

# Use a local Ollama model (no API key needed)
claurst --provider ollama --model llama3.2 "your prompt"

# Or via environment variable
CLAURST_PROVIDER=google claurst "your prompt"
```

---

## Local Models (No API Key)

Providers that run locally require no API key:

**Ollama:**

```bash
# Install Ollama from https://ollama.ai and pull a model
ollama pull llama3.2

# Run Claurst against it
claurst --provider ollama --model llama3.2
```

**LM Studio:**

```bash
# Start the LM Studio local server (default port 1234)
claurst --provider lmstudio
```

**llama.cpp server:**

```bash
claurst --provider llamacpp --api-base http://localhost:8080
```

---

## Security Recommendations

- Store API keys in environment variables or a secrets manager rather than in
  `settings.json`, especially on shared or CI systems.
- Restrict permissions on `~/.claurst/` to your user only:
  ```bash
  chmod 700 ~/.claurst
  chmod 700 ~/.claurst/accounts
  chmod 600 ~/.claurst/accounts.json
  chmod 600 ~/.claurst/auth.json
  chmod 600 ~/.claurst/settings.json
  find ~/.claurst/accounts -type f -name '*tokens.json' -exec chmod 600 {} +
  ```
  Claurst already sets `0600` on `accounts.json` automatically on Unix; the
  command above is the belt-and-braces version that also covers the per-
  profile token files.
- Do not commit `~/.claurst/` to version control.
- Add `.claurst/` to your project's `.gitignore` to prevent accidentally
  committing project-level settings files that may contain keys.
- Rotate API keys periodically from the Anthropic Console.
- Use `claurst auth logout` on shared machines before logging out of your
  user session.
