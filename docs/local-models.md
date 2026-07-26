# Running Claurst against local models

Claurst can drive any server that speaks the **OpenAI-compatible chat
completions API** (`POST /v1/chat/completions`). That includes:

- **llama.cpp** (`llama-server`) — provider id `llamacpp`
- **LM Studio** — provider id `lmstudio`
- **Ollama** (via its `/v1` OpenAI shim) — provider id `ollama`
- **vLLM**, **text-generation-webui**, **LocalAI**, and anything else that
  exposes `/v1/chat/completions` — use the generic `openai` provider with a
  custom base URL

No API key is required for a local server. The rest of this page focuses on
`llama-server` because it exposes the most tuning knobs, but the caching and
model guidance apply to every OpenAI-compatible backend.

For the short per-provider reference (env vars, default ports), see
[Providers](providers). This page is the practical guide to getting an
agentic loop working locally.

---

## Connecting

### llama.cpp (`llama-server`)

Start the server, then point Claurst at it. The built-in `llamacpp` provider
reads `LLAMA_CPP_HOST` (default `http://localhost:8080`) and appends `/v1`:

```bash
# Terminal 1 — start the model server
llama-server -m ./models/your-model.gguf --host 127.0.0.1 --port 8080 --jinja

# Terminal 2 — run Claurst against it
claurst --provider llamacpp --model your-model "add a health-check endpoint"
```

If your server runs elsewhere:

```bash
LLAMA_CPP_HOST=http://192.168.1.50:8080 claurst --provider llamacpp --model your-model
```

Or persist it in `~/.claurst/settings.json`:

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

`--model` is the label the server advertises at `/v1/models`; llama-server
usually reports the GGUF filename. Any non-empty string works if the server
ignores it.

### Any other OpenAI-compatible server

LM Studio and Ollama have dedicated provider ids (`lmstudio`, `ollama`) — see
[Providers](providers). For everything else, use the generic `openai` provider
and override the base URL:

```bash
OPENAI_BASE_URL=http://localhost:8000/v1 \
  claurst --provider openai --model my-model "..."
```

Claurst posts to `{base_url}/v1/chat/completions`, so set the base URL to the
host root (Claurst appends `/v1`) or to a value already ending in `/v1`
depending on the provider — match the examples above.

---

## Recommended `llama-server` flags for agentic use

Agentic coding is more demanding than chat: the model has to plan across turns
and emit **valid tool calls** every step. These flags matter most.

### `--jinja` — required for tool calling

```bash
llama-server -m model.gguf --jinja
```

`--jinja` tells llama-server to render the **chat template embedded in the
GGUF** instead of a generic fallback. Tool calling only works when that
template knows how to format the `tools` list and parse the model's
`tool_calls` back out. Without `--jinja` most models will either ignore the
tools Claurst sends or emit tool calls as plain text that never get executed —
which looks like "the model thinks forever but never edits any files."

If a model's built-in template lacks tool support, pass a template that has it
with `--chat-template <name>` or `--chat-template-file <path>`.

### Context size and the `--parallel` gotcha

```bash
llama-server -m model.gguf --ctx-size 32768 --parallel 1
```

- `--ctx-size` / `-c` is the **total** KV context across all slots. The default
  (often 4096) is too small for agentic work — a single file read plus the
  system prompt can blow past it. Give it as much as your VRAM/RAM allows.
- `--parallel` / `-np` is the number of concurrent slots, and **the context is
  divided evenly among them**. `--ctx-size 32768 --parallel 4` gives each
  request only **8192** tokens, not 32768. For a single interactive Claurst
  session use `--parallel 1` (or size `--ctx-size` as `N × per-request-window`).
  Setting a big parallel count with a modest ctx-size is a common cause of "the
  model keeps losing context" and of a tiny effective prompt cache.

Claurst also auto-compacts the conversation as it approaches the window (see
`auto_compact` / `compact_threshold` in [Configuration](configuration)), so
you generally want the server window comfortably larger than a couple of tool
round-trips.

### Prompt caching (prefix reuse)

Prompt caching is what makes multi-turn agentic loops affordable: each turn
Claurst re-sends the whole growing conversation, and the server should reuse
the KV cache for the unchanged prefix instead of recomputing it.

- llama-server reuses the cached prompt prefix **by default** (the `cache_prompt`
  request field, which Claurst relies on) — you usually don't have to enable
  anything. The `--cache-prompt` flag some guides mention is accepted but not
  required.
- `--cache-reuse N` (a.k.a. `-cru`) lets the server reuse cached chunks even
  after a small divergence of at least `N` tokens. This helps when an edit
  changes text in the middle of the conversation; without it, everything after
  the first changed token is recomputed.
- **Recent builds (llama.cpp >= b4600) report the reused count** as
  `usage.prompt_tokens_details.cached_tokens`. Claurst reads that field, so
  cache hits show up in `/usage` and `/extra-usage`. Older builds don't emit
  it — caching may still be working, the server just isn't reporting numbers
  (see [Cache accounting](#cache-accounting-what-the-numbers-mean) below).

Verify the server is reporting cache hits with two identical requests:

```bash
curl -s http://localhost:8080/v1/chat/completions \
  -H 'Content-Type: application/json' \
  -d '{"model":"m","messages":[{"role":"user","content":"hi"}]}' | \
  python -c 'import sys,json; print(json.load(sys.stdin)["usage"])'
```

On the second call, `prompt_tokens_details.cached_tokens` should be non-zero.
If the key is absent, your build predates cache-usage reporting.

### `--no-context-shift` — understand the tradeoff

By default, when a request exceeds a slot's context, llama-server **shifts** the
window: it drops the oldest tokens and keeps going. For an agent that is bad in
two ways — it silently discards earlier reasoning/instructions, and it changes
the prompt prefix, which **invalidates the prompt cache** from that point on.

`--no-context-shift` disables shifting: instead of dropping tokens the server
returns an error when the context is full. That keeps the cache prefix stable
and never silently forgets context, but it means you must keep `--ctx-size`
large enough and let Claurst's compaction manage history. Recommended for
agentic use, paired with a generous context and `auto_compact` enabled.

### Performance flags

These don't affect correctness but help throughput and memory:

```bash
llama-server -m model.gguf \
  --jinja --ctx-size 32768 --parallel 1 --no-context-shift \
  --flash-attn on \
  --cache-type-k q8_0 --cache-type-v q8_0 \
  --n-gpu-layers 999 --mlock \
  --host 127.0.0.1 --port 8080 --no-webui
```

- `--flash-attn on` (`-fa`) — lower KV memory, usually faster.
- `--cache-type-k` / `--cache-type-v` — quantize the KV cache (`q8_0` is a good
  memory/quality balance; `f16` is full precision).
- `--n-gpu-layers` (`-ngl`) — offload as many layers to GPU as fit.
- `--mlock` — keep weights resident in RAM.
- `--no-webui` — skip the bundled web UI.

---

## Why tool calling needs a tool-aware chat template

Claurst is agentic: it sends the model a list of `tools` and expects structured
`tool_calls` back so it can read files, run shell commands, and edit code. That
handshake depends on the model's **chat template** encoding tool definitions and
tool-call syntax. Two things have to line up:

1. The model was **trained** for tool/function calling.
2. The GGUF ships a **chat template that implements it**, and you enabled it
   with `--jinja` (or supplied one via `--chat-template`).

If either is missing, the model may narrate what it "would" do instead of
emitting a real tool call — the classic "runs for a long time thinking but never
changes any files" symptom. Base/completion models (no instruct/chat tuning)
don't do tool calling at all.

---

## Model guidance

There is no single "best" local model — it depends on your hardware — but for
agentic coding, prioritize **reliable tool calling** over raw benchmark scores.
General, hardware-agnostic guidance (not benchmarks):

- **Use an instruct/chat model with native tool calling**, not a base model.
  Families that ship tool-aware templates and generally handle function calling
  include Qwen2.5 / Qwen2.5-Coder Instruct, Qwen3 Instruct, Llama 3.1 / 3.3
  Instruct, Mistral-Small and Devstral (coding/agent focused), Hermes 3, and
  Functionary. Confirm the specific GGUF you download has a tool-capable
  template.
- **Bigger dense or coder-tuned models are steadier.** Multi-step tool loops are
  hard; 30B-class-and-up dense instruct models, or coder-specialized ones, tend
  to loop less than very small models.
- **Watch MoE models with tiny active parameter counts.** A model advertised as
  "A3B" activates ~3B parameters per token; those can over-think or loop on
  agentic tasks even if the total parameter count is large. If a model spends
  every turn "thinking" and never calls a tool, it may simply not be strong
  enough at agentic tool use, or its template's reasoning handling isn't
  compatible — try a different model or a coder-tuned variant.
- **Don't over-quantize.** Q4_K_M and up is a common sweet spot; Q2/Q3 quants
  noticeably degrade tool-call reliability.

Community fine-tunes vary a lot: two GGUFs of the "same" base model can behave
very differently depending on whose template and tuning they ship. When in
doubt, start from an official instruct release to confirm the loop works, then
swap in fine-tunes.

---

## Cache accounting: what the numbers mean

A few UI details confuse people running local models:

- **The `0k/262k` counter in the status bar is the _context window_ counter, not
  a cache counter.** It shows `tokens-in-context / model-context-window`. It
  advances as the conversation grows and resets/drops when you `/compact`. If it
  is stuck at `0k`, no real turns have completed — commonly because prompts
  aren't reaching the model (for example a separate pasted-text bug, fixed on
  its own), not because caching is off.
- **Cache read/write live in `/usage` and `/extra-usage`.** Claurst populates
  them from `usage.prompt_tokens_details.cached_tokens` when the server reports
  it. If your server never reports cache info, those lines show **`n/a`** rather
  than a permanent `0`, so you can tell "no cache data reported" apart from "zero
  cache hits." Upgrade to a recent llama.cpp build (>= b4600) to get real
  numbers.
- Local models are priced at `$0.00`, so the cost line stays at zero regardless
  of cache activity — that's expected, not a bug.

---

## Is `CLAURST_COORDINATOR_MODE=1` a real thing?

Short answer: **the name exists in the source, but setting it currently does
nothing — don't cargo-cult it for local models (or any models).**

Details, from grepping this repository:

- The string `CLAURST_COORDINATOR_MODE` appears exactly once in the Rust
  runtime, as a constant in `crates/query/src/coordinator.rs`. There is a
  `coordinator` module (multi-worker orchestration) and a [Agents](agents) doc
  page describing a coordinator/worker model.
- **But nothing in the live agent loop reads it.** `is_coordinator_mode()` (the
  function that would check the env var) has no callers outside the module's own
  unit tests, and the system-prompt builder hardcodes `coordinator_mode: false`.
  So exporting `CLAURST_COORDINATOR_MODE=1` before launching Claurst has no
  observable effect in this version.
- **You don't need it anyway.** Claurst is agentic by default — it plans and
  calls tools on every run. There is no separate "enable agentic mode" switch to
  flip. If an assistant told you to set `CLAURST_COORDINATOR_MODE=1` to "turn on
  agentic workflows," that advice was wrong.

If you specifically want parallel sub-agent orchestration, see
[Agents](agents) and [`/managed-agents`](commands) for what is actually wired
up today. Note that orchestration multiplies context and demands strong,
reliable tool calling, so it is generally a poor fit for small local models.

---

## Quick troubleshooting

| Symptom | Likely cause / fix |
|---------|--------------------|
| Model "thinks" forever, never edits files | Missing `--jinja`, or the model/template doesn't support tool calls. Try a tool-capable instruct model. |
| Context fills almost immediately | `--parallel N` is dividing `--ctx-size` across slots. Use `--parallel 1` or raise `--ctx-size`. |
| Cache read shows `n/a` in `/usage` | Server isn't reporting `cached_tokens`. Upgrade llama.cpp (>= b4600); caching may still be working. |
| `0k/262k` never moves | No completed turns — check that prompts actually reach the model, not a caching problem. |
| Errors when context fills (with `--no-context-shift`) | Expected — raise `--ctx-size` and enable Claurst `auto_compact`. |
| Tool calls arrive as plain text | Template isn't tool-aware; pass `--chat-template`/`--chat-template-file` or pick another model. |
