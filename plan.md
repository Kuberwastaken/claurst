# Managed Agents Implementation Plan

## 1. Overview

Managed agents introduces a **manager-executor architecture** to Claurst where a
larger, more capable "manager" model (e.g., Claude Opus, Gemini Pro) orchestrates
work by delegating tasks to smaller, cheaper "executor" models (e.g., Claude
Sonnet, Gemini Flash). The manager reasons about what needs to be done, breaks
work into sub-tasks, spawns executor agents via the existing `AgentTool`, reviews
their results, and synthesizes a final answer.

This is distinct from the existing coordinator mode (`coordinator.rs`) which is a
simpler env-var-gated mode. Managed agents are **user-configured**, support
**cross-provider** combinations, enforce **budget splitting**, and are activated
via the `/managed-agents` slash command.

**Key principle:** Reuse the existing `AgentTool` and `run_query_loop`
infrastructure rather than building a parallel orchestration system. The manager
is simply a query loop whose system prompt instructs it to delegate via
`AgentTool`, and `AgentTool` already supports per-agent model overrides.

## 2. Architecture

### 2.1 High-Level Message Flow

```
User Input
    |
    v
+-------------------+
|   Manager Model   |  (Opus / Pro / o1 — configured via /managed-agents)
|  system prompt:   |
|  "You delegate    |
|   to executors"   |
+-------------------+
    |                          |                          |
    | AgentTool(model=sonnet)  | AgentTool(model=flash)   | AgentTool(model=sonnet)
    v                          v                          v
+-----------+          +-----------+              +-----------+
| Executor 1|          | Executor 2|              | Executor 3|
| (Sonnet)  |          | (Flash)   |              | (Sonnet)  |
+-----------+          +-----------+              +-----------+
    |                          |                          |
    +----------+---------------+--------------------------+
               |
               v
+-------------------+
|   Manager Model   |  Synthesizes results, may spawn more executors
+-------------------+
               |
               v
         Final Response
```

### 2.2 Component Interaction

```
settings.json                          /managed-agents command
     |                                        |
     v                                        v
ManagedAgentConfig  <----  save_settings_mutation()
     |
     v
QueryConfig.managed_agents: Option<ManagedAgentConfig>
     |
     +---> run_query_loop() [manager model]
               |
               +---> System prompt includes managed-agent delegation instructions
               +---> AgentTool.execute() uses executor model from config
               +---> CostTracker shared (Arc) between manager and all executors
               +---> Budget checks: manager_budget + executor_budget <= total
```

### 2.3 Key Design Decisions

1. **Manager IS the query loop** — no new loop type. The manager's `QueryConfig`
   uses the manager model; executor spawns use `AgentTool` with the executor
   model override.

2. **Config flows through `QueryConfig`** — a new `managed_agents` field carries
   the `ManagedAgentConfig`. The `AgentTool` reads it from `ToolContext` to
   default executor model/provider when the manager doesn't specify one.

3. **Budget is shared** — the existing `Arc<CostTracker>` already propagates to
   sub-agents. We add a budget-split policy that allocates a percentage to the
   manager vs. executors.

4. **Provider resolution uses `ProviderRegistry`** — already supports multiple
   simultaneous providers. Cross-provider combos work because each
   `run_query_loop` call resolves its own provider via
   `QueryConfig.provider_registry`.

---

## 3. Implementation Phases

### Phase 1: Configuration and Data Structures

**Goal:** Define the `ManagedAgentConfig` type, persist it in settings, and wire
it into `QueryConfig`.

**Dependencies:** None (foundational phase).

#### Files to modify

- **`src-rust/crates/core/src/lib.rs`**
  - Add `ManagedAgentConfig` struct near line 628 (after `AgentDefinition`):
    ```
    pub struct ManagedAgentConfig { ... }
    pub struct ManagedAgentPreset { ... }
    pub enum BudgetSplitPolicy { ... }
    ```
  - Add `managed_agents: Option<ManagedAgentConfig>` field to `Config` (line ~759)
  - Add `managed_agents: Option<ManagedAgentConfig>` field to `Settings` (line ~862)

- **`src-rust/crates/query/src/lib.rs`**
  - Add `managed_agents: Option<ManagedAgentConfig>` to `QueryConfig` (line ~123)
  - Update `QueryConfig::default()` to include `managed_agents: None` (line ~148)
  - Update `QueryConfig::from_config()` to propagate managed_agents from `Config`

#### Data structures

```rust
/// Budget allocation strategy between manager and executor agents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BudgetSplitPolicy {
    /// Fixed percentage to manager, rest to executors. E.g., 30/70.
    Percentage { manager_pct: u8 },
    /// Hard USD caps for each role.
    FixedCaps { manager_usd: f64, executor_usd: f64 },
    /// No split — shared pool (default).
    SharedPool,
}

/// Configuration for the managed-agent (manager-executor) architecture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedAgentConfig {
    /// Whether managed-agent mode is active.
    pub enabled: bool,
    /// Provider/model for the manager (e.g., "anthropic/claude-opus-4").
    pub manager_model: String,
    /// Provider/model for executors (e.g., "anthropic/claude-sonnet-4").
    pub executor_model: String,
    /// Maximum turns each executor sub-agent may take.
    pub executor_max_turns: u32,
    /// Maximum number of concurrent executor agents.
    pub max_concurrent_executors: u32,
    /// Budget allocation strategy.
    pub budget_split: BudgetSplitPolicy,
    /// Optional total USD budget cap for the entire managed session.
    pub total_budget_usd: Option<f64>,
    /// Name of the preset used (for display), or "custom".
    pub preset_name: Option<String>,
    /// Whether executors should use worktree isolation by default.
    pub executor_isolation: bool,
}

/// A named preset for common manager-executor configurations.
#[derive(Debug, Clone)]
pub struct ManagedAgentPreset {
    pub name: &'static str,
    pub description: &'static str,
    pub manager_model: &'static str,
    pub executor_model: &'static str,
    pub executor_max_turns: u32,
    pub max_concurrent_executors: u32,
}
```

#### Built-in presets

| Preset Name | Manager Model | Executor Model | Notes |
|---|---|---|---|
| `anthropic-tiered` | `anthropic/claude-opus-4` | `anthropic/claude-sonnet-4` | Same-provider, cost-optimized |
| `google-tiered` | `google/gemini-2.5-pro` | `google/gemini-2.5-flash` | Same-provider Google |
| `cross-opus-flash` | `anthropic/claude-opus-4` | `google/gemini-2.5-flash` | Cross-provider, cheapest executors |
| `cross-pro-sonnet` | `google/gemini-2.5-pro` | `anthropic/claude-sonnet-4` | Cross-provider alternative |
| `budget` | `anthropic/claude-sonnet-4` | `anthropic/claude-haiku-4` | Lowest cost combo |
| `custom` | (user picks) | (user picks) | Interactive setup |

#### Checklist

- [ ] Define `ManagedAgentConfig` struct with serde derives
- [ ] Define `BudgetSplitPolicy` enum
- [ ] Define `ManagedAgentPreset` struct and `fn builtin_presets() -> Vec<ManagedAgentPreset>`
- [ ] Add `managed_agents` field to `Config` struct (line ~759)
- [ ] Add `managed_agents` field to `Settings` struct (line ~862)
- [ ] Add `managed_agents` field to `QueryConfig` struct (line ~123)
- [ ] Wire `from_config()` to propagate managed_agents
- [ ] Add unit tests for serialization/deserialization round-trip
- [ ] Add unit tests for preset generation

---

### Phase 2: `/managed-agents` Slash Command

**Goal:** Create the user-facing command for configuring managed agent mode.

**Dependencies:** Phase 1 (data structures must exist).

#### Files to create/modify

- **`src-rust/crates/commands/src/lib.rs`**
  - Add `ManagedAgentsCommand` struct (new section, ~50 lines)
  - Register `Box::new(ManagedAgentsCommand)` in `all_commands()` (line ~7691)
  - Uses `save_settings_mutation()` (line ~211) for persistence

#### Subcommands

| Subcommand | Usage | Behavior |
|---|---|---|
| (no args) / `status` | `/managed-agents` | Show current config or "not configured" |
| `setup` | `/managed-agents setup` | Interactive: pick preset or custom |
| `presets` | `/managed-agents presets` | List all available presets |
| `configure <key> <value>` | `/managed-agents configure executor-model anthropic/claude-haiku-4` | Modify a single field |
| `preset <name>` | `/managed-agents preset anthropic-tiered` | Apply a named preset |
| `disable` | `/managed-agents disable` | Set `enabled: false`, keep config |
| `enable` | `/managed-agents enable` | Set `enabled: true` (must have been set up) |
| `budget <amount>` | `/managed-agents budget 5.00` | Set total USD budget |

#### Command implementation outline

```rust
pub struct ManagedAgentsCommand;

#[async_trait]
impl SlashCommand for ManagedAgentsCommand {
    fn name(&self) -> &str { "managed-agents" }
    fn aliases(&self) -> Vec<&str> { vec!["ma"] }
    fn description(&self) -> &str {
        "Configure manager-executor agent architecture"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        // Parse subcommand from args
        // Dispatch to setup/status/presets/configure/disable handlers
        // Return ConfigChangeMessage or Message
    }
}
```

#### Status display format

```
Managed Agents: ACTIVE
  Manager:   anthropic/claude-opus-4
  Executor:  anthropic/claude-sonnet-4
  Preset:    anthropic-tiered
  Budget:    $5.00 (shared pool)
  Executor limits: 10 turns, 4 concurrent, worktree isolation OFF
  Session cost: $1.23 (manager: $0.89 / executors: $0.34)
```

#### Checklist

- [ ] Create `ManagedAgentsCommand` struct implementing `SlashCommand`
- [ ] Implement `status` subcommand (show current config)
- [ ] Implement `presets` subcommand (list presets with descriptions)
- [ ] Implement `preset <name>` subcommand (apply preset)
- [ ] Implement `setup` subcommand (interactive flow)
- [ ] Implement `configure <key> <value>` subcommand
- [ ] Implement `enable` / `disable` subcommands
- [ ] Implement `budget <amount>` subcommand
- [ ] Register in `all_commands()` at line ~7691
- [ ] Persist config via `save_settings_mutation()`
- [ ] Return `ConfigChangeMessage` so the live `Config` updates
- [ ] Add help text with examples
- [ ] Add tests for argument parsing

---

### Phase 3: Manager-Executor Orchestration Engine

**Goal:** When managed-agent mode is enabled, transform the query loop so the
manager model delegates to executors via `AgentTool`.

**Dependencies:** Phase 1 (config), Phase 2 (command, for activation).

#### Files to create/modify

- **`src-rust/crates/query/src/managed_orchestrator.rs`** (NEW)
  - Manager system prompt construction
  - Executor model injection into `AgentTool`
  - Budget enforcement logic
  - Cost attribution (manager vs. executor)

- **`src-rust/crates/query/src/lib.rs`**
  - Add `pub mod managed_orchestrator;` near line 17
  - In `run_query_loop()` (line ~663): at the start, if
    `config.managed_agents.is_some() && config.managed_agents.enabled`, apply the
    managed-agent system prompt and set the model to the manager model

- **`src-rust/crates/query/src/agent_tool.rs`**
  - In `AgentTool::execute()` (line ~221): when `ToolContext` carries a
    `ManagedAgentConfig` and the user did NOT specify a model override, default
    to `config.executor_model`
  - Apply `executor_max_turns` from config when `max_turns` not explicitly set
  - Apply `executor_isolation` default from config

- **`src-rust/crates/tools/src/lib.rs`** (or wherever `ToolContext` is defined)
  - Add `managed_agent_config: Option<ManagedAgentConfig>` to `ToolContext`

#### Manager system prompt

The orchestrator module provides a function that generates a system prompt
section injected when managed-agent mode is active:

```rust
pub fn managed_agent_system_prompt(config: &ManagedAgentConfig) -> String {
    format!(r#"
## Managed Agent Mode

You are operating as the MANAGER in a manager-executor architecture.

### Your Role
- You are the planning and reasoning layer. You do NOT execute tasks directly.
- Delegate all implementation work to executor agents using the Agent tool.
- Each executor runs {executor_model} and has up to {max_turns} turns.
- You may run up to {max_concurrent} executors in parallel using run_in_background.

### Workflow
1. Analyze the user's request and break it into sub-tasks.
2. Spawn executor agents for each sub-task (use the Agent tool).
3. Review executor results — if insufficient, spawn follow-up executors.
4. Synthesize all results into a coherent response to the user.

### Executor Configuration
- Model: {executor_model}
- Max turns per executor: {max_turns}
- Isolation: {isolation}
- Always provide fully self-contained prompts (executors cannot see your context).

### Budget
- Total budget: {budget}
- Spent so far: (injected at runtime)
- Be cost-conscious: prefer fewer, well-scoped executors over many small ones.
"#, ...)
}
```

#### Budget enforcement

```rust
pub struct ManagedBudgetTracker {
    config: ManagedAgentConfig,
    manager_cost: Arc<CostTracker>,    // same Arc shared with loop
    executor_cost_sum: AtomicF64,      // accumulated from executor sub-agents
}

impl ManagedBudgetTracker {
    /// Check if the next executor spawn would exceed the budget policy.
    pub fn can_spawn_executor(&self) -> bool { ... }
    /// Record cost from a completed executor.
    pub fn record_executor_cost(&self, cost_usd: f64) { ... }
    /// Get breakdown for display.
    pub fn breakdown(&self) -> (f64, f64, f64) { ... } // (manager, executors, total)
}
```

The `CostTracker` is already `Arc`-shared between parent and sub-agents (see
`agent_tool.rs`). Budget enforcement hooks into the existing
`QueryOutcome::BudgetExceeded` path — the managed orchestrator checks the split
policy before each executor spawn and injects a budget warning into the manager's
context when limits are approaching.

#### Checklist

- [ ] Create `src-rust/crates/query/src/managed_orchestrator.rs`
- [ ] Implement `managed_agent_system_prompt()` function
- [ ] Implement `ManagedBudgetTracker` struct
- [ ] Add `managed_agent_config` to `ToolContext`
- [ ] Modify `AgentTool::execute()` to default to executor model from config
- [ ] Modify `AgentTool::execute()` to apply executor_max_turns default
- [ ] Modify `AgentTool::execute()` to apply executor_isolation default
- [ ] In `run_query_loop()`: inject managed-agent system prompt when enabled
- [ ] In `run_query_loop()`: override model to manager_model when enabled
- [ ] In `run_query_loop()`: check budget split before each tool execution
- [ ] Inject real-time cost into manager context between turns
- [ ] Add `pub mod managed_orchestrator` to `lib.rs`
- [ ] Unit tests for system prompt generation
- [ ] Unit tests for budget split calculations
- [ ] Integration test: manager spawns executor, gets result

---

### Phase 4: TUI Integration

**Goal:** Display manager/executor status, cost breakdown, and progress in the
terminal UI.

**Dependencies:** Phase 3 (orchestrator must be functional).

#### Files to modify

- **`src-rust/crates/tui/src/agents_view.rs`**
  - Extend `AgentInfo` (line ~56) with:
    - `agent_role: AgentRole` (enum: `Manager`, `Executor`, `Normal`)
    - `model_name: Option<String>`
    - `cost_usd: f64`
  - New `AgentRole` enum with distinct styling:
    - Manager: bold + accent color
    - Executor: dimmed, indented under manager
  - Update the `render_agents_panel` to show role badges and cost

- **`src-rust/crates/tui/src/lib.rs`** (or `app.rs`)
  - Extend `App.agent_status` to carry `AgentRole`
  - Add `managed_agent_cost_breakdown: Option<(f64, f64, f64)>` to `App`
  - Render cost breakdown in the status bar / cost panel

- **`src-rust/crates/tui/src/agents_view.rs`**
  - In the agents list widget (line ~96+), render:
    ```
    [MANAGER] claude-opus-4      running    $0.89
      [EXEC] claude-sonnet-4 #1  complete   $0.12  (5 turns)
      [EXEC] claude-sonnet-4 #2  running    $0.08  (3 turns)
      [EXEC] gemini-flash #3     waiting    $0.02  (1 turn)
    ```

#### Checklist

- [ ] Add `AgentRole` enum to `agents_view.rs`
- [ ] Add `agent_role`, `model_name`, `cost_usd` fields to `AgentInfo`
- [ ] Update agent list rendering with role badges and indentation
- [ ] Add cost breakdown display (manager vs. executor costs)
- [ ] Add visual progress indicators for active executors
- [ ] Wire `ManagedBudgetTracker::breakdown()` to TUI refresh cycle
- [ ] Show "Managed Mode" badge in the main status line when active
- [ ] Add keyboard shortcut to toggle managed-agents panel visibility

---

### Phase 5: Session Persistence

**Goal:** Record managed-agent sessions so they can be reviewed and resumed.

**Dependencies:** Phase 3 (orchestrator running).

#### Files to modify

- **`src-rust/crates/core/src/session_storage.rs`**
  - Extend `TranscriptMessage` (line ~103) with:
    - `agent_role: Option<String>` — `"manager"`, `"executor"`, or absent
    - `managed_session_id: Option<String>` — links executor entries to their
      manager session
  - These fields are `#[serde(skip_serializing_if = "Option::is_none")]` so they
    don't break existing sessions

- **`src-rust/crates/query/src/managed_orchestrator.rs`**
  - When writing transcript entries for manager turns, set `agent_role = "manager"`
  - When AgentTool spawns executors, the executor's transcript entries get
    `agent_role = "executor"` and `is_sidechain = true` (already the case)

- **`src-rust/crates/core/src/session_storage.rs`**
  - In `load_transcript()` (line ~282): no changes needed (new fields are
    optional, backwards compatible)
  - Add `filter_by_agent_role()` helper for UI display

#### Session resume

When resuming a managed-agent session:
1. Load the transcript as normal
2. Detect `managed_agents` in the session's config snapshot
3. Re-activate managed-agent mode with the stored config
4. Manager context is restored; executor sidechains are available for reference

#### Checklist

- [ ] Add `agent_role` field to `TranscriptMessage`
- [ ] Add `managed_session_id` field to `TranscriptMessage`
- [ ] Set agent_role in manager turn transcript writes
- [ ] Set agent_role in executor turn transcript writes
- [ ] Add `filter_by_agent_role()` helper
- [ ] Test: write and read back a managed-agent session
- [ ] Test: resume a managed-agent session restores config
- [ ] Ensure backwards compatibility with existing JSONL files

---

### Phase 6: Testing

**Goal:** Comprehensive test coverage for all managed-agent functionality.

**Dependencies:** All prior phases.

#### Test categories

**Unit tests** (`src-rust/crates/core/src/lib.rs`, test module):
- [ ] `ManagedAgentConfig` serialization round-trip
- [ ] `BudgetSplitPolicy` variants serialize correctly
- [ ] Preset generation returns expected configs
- [ ] Budget split calculations for all policy variants
- [ ] Config merge: managed_agents propagates through Settings -> Config -> QueryConfig

**Unit tests** (`src-rust/crates/commands/src/lib.rs`, test module):
- [ ] `/managed-agents status` with no config
- [ ] `/managed-agents preset anthropic-tiered` applies correct config
- [ ] `/managed-agents configure executor-model X` updates single field
- [ ] `/managed-agents enable` / `disable` toggle
- [ ] `/managed-agents budget 5.00` sets budget
- [ ] Invalid subcommand returns helpful error

**Unit tests** (`src-rust/crates/query/src/managed_orchestrator.rs`):
- [ ] System prompt generation includes correct model names
- [ ] Budget tracker: `can_spawn_executor()` with various policies
- [ ] Budget tracker: `record_executor_cost()` accumulates correctly
- [ ] Budget tracker: breakdown percentages

**Integration tests** (`src-rust/crates/query/tests/` or inline):
- [ ] Manager model receives managed-agent system prompt
- [ ] AgentTool defaults to executor_model when managed config present
- [ ] AgentTool respects explicit model override even in managed mode
- [ ] Budget exceeded stops executor spawning
- [ ] Cross-provider: Anthropic manager + Google executor resolves correctly
- [ ] Session transcript contains correct agent_role annotations

**Provider compatibility tests**:
- [ ] Anthropic -> Anthropic (same provider)
- [ ] Google -> Google (same provider)
- [ ] Anthropic -> Google (cross-provider)
- [ ] Google -> Anthropic (cross-provider)
- [ ] OpenAI-compat -> Anthropic (cross-family)
- [ ] Verify both providers authenticated before enabling

---

## 4. Risk Assessment

### High Risk

| Risk | Impact | Mitigation |
|---|---|---|
| **Runaway cost** — manager spawns many executors | Could burn through budget quickly | Budget split policy enforced before each spawn; hard cap via `total_budget_usd`; real-time cost injection into manager context |
| **Infinite delegation** — manager spawns executor that tries to spawn sub-executor | Stack overflow or cost spiral | `AgentTool` already excludes itself from sub-agent tools (line 260 of agent_tool.rs); executor's tool list never includes `AgentTool` |
| **Cross-provider auth failure** — user configures a combo where one provider has no key | Silent failure at executor spawn time | Validate both provider auth during `/managed-agents setup`; fail fast with clear error message |

### Medium Risk

| Risk | Impact | Mitigation |
|---|---|---|
| **Context mismatch** — executor doesn't have enough context because manager's prompt was too terse | Low-quality executor output | Manager system prompt emphasizes self-contained prompts; provide examples |
| **Model capability gap** — executor model can't handle the delegated task | Failed sub-task | Manager reviews executor output and can retry with different instructions or escalate |
| **Rate limits** — multiple concurrent executors hit provider rate limits | Executor failures | Respect `max_concurrent_executors`; AgentTool already handles rate-limit errors via fallback |
| **Worktree conflicts** — multiple executors editing overlapping files | Merge conflicts | Worktree isolation is already implemented in `AgentTool`; make it default-on for managed mode |

### Low Risk

| Risk | Impact | Mitigation |
|---|---|---|
| **Settings migration** — existing settings.json has no `managed_agents` field | Deserialization failure | All new fields use `#[serde(default)]` and `Option<T>` — safe for existing files |
| **Session compat** — old sessions have no `agent_role` field | Missing metadata on resume | `agent_role` is `Option<String>` with `skip_serializing_if`, backwards compatible |

---

## 5. Cost Considerations

### Budget Split Policies

**SharedPool (default):** Manager and executors draw from one `CostTracker`. The
existing `max_budget_usd` in `QueryConfig` applies to the total. Simple but
offers no guarantees about balance.

**Percentage:** Manager gets N% of the budget, executors get (100-N)%.
Recommended default: 30% manager / 70% executors (manager reasons, executors do
heavy lifting).

**FixedCaps:** Hard USD limits per role. Useful for predictable billing.

### Estimated cost per interaction (Anthropic pricing, April 2026)

| Scenario | Manager Tokens | Executor Tokens | Est. Cost |
|---|---|---|---|
| Simple refactor (1 executor, 5 turns) | ~5K in / 2K out (Opus) | ~20K in / 10K out (Sonnet) | ~$0.15 |
| Feature implementation (3 executors, 10 turns each) | ~15K in / 5K out (Opus) | ~180K in / 60K out (Sonnet) | ~$1.20 |
| Large codebase analysis (5 executors, parallel) | ~20K in / 8K out (Opus) | ~500K in / 100K out (Sonnet) | ~$3.00 |
| Budget combo (Sonnet manager, Haiku executors) | ~10K in / 3K out (Sonnet) | ~200K in / 50K out (Haiku) | ~$0.20 |

### Cost-saving strategies built into the design

1. Manager system prompt emphasizes batching work into fewer, larger executor tasks
2. Budget injection into manager context creates cost awareness
3. `can_spawn_executor()` gate prevents overspending
4. Presets guide users toward cost-effective combos
5. `/managed-agents budget` provides a hard stop

---

## 6. Provider Compatibility Matrix

Authentication must be verified for both the manager and executor providers before
managed-agent mode can be activated.

| Manager Provider | Executor Provider | Status | Notes |
|---|---|---|---|
| Anthropic | Anthropic | Full support | Same auth, same registry entry |
| Google | Google | Full support | Same auth |
| Anthropic | Google | Full support | Both in ProviderRegistry, separate auth |
| Google | Anthropic | Full support | Both in ProviderRegistry, separate auth |
| OpenAI | Anthropic | Full support | Standard cross-provider |
| OpenAI | OpenAI | Full support | Same auth |
| Anthropic | OpenAI-compat (Groq, etc.) | Full support | Any registered provider works |
| Any | Local (llama-cpp, Ollama, LM Studio) | Supported | No auth needed for executor; latency may be high |
| Bedrock | Anthropic | Supported | Bedrock as manager, direct API as executor |
| Any | Any (30+ providers) | Supported | ProviderRegistry already handles all combos |

### Cross-provider constraints

- Both providers must be registered in `ProviderRegistry` (line 21, `registry.rs`)
- Both must have valid credentials in `AuthStore` (`~/.claurst/auth.json`)
- Model names must be resolvable via `ModelRegistry::resolve("provider/model")`
- Streaming format differences are abstracted by the `LlmProvider` trait — no
  special handling needed

### Providers that may have issues

| Provider | Concern | Workaround |
|---|---|---|
| Bedrock / Azure | IAM/AD auth more complex than API keys | Validate auth during setup, not at spawn time |
| Local providers | No cost tracking (pricing is $0) | Budget split is meaningless; disable budget enforcement |
| Rate-limited free tiers | Concurrent executors may hit limits fast | Lower `max_concurrent_executors` for these providers |

---

## 7. File Change Summary

| File | Phase | Change Type | Description |
|---|---|---|---|
| `src-rust/crates/core/src/lib.rs` | 1 | Modify | Add `ManagedAgentConfig`, `BudgetSplitPolicy`, presets; add fields to `Config` and `Settings` |
| `src-rust/crates/query/src/lib.rs` | 1, 3 | Modify | Add `managed_agents` to `QueryConfig`; inject managed system prompt in `run_query_loop()` |
| `src-rust/crates/commands/src/lib.rs` | 2 | Modify | Add `ManagedAgentsCommand`; register in `all_commands()` |
| `src-rust/crates/query/src/managed_orchestrator.rs` | 3 | **Create** | Manager system prompt, `ManagedBudgetTracker`, cost attribution |
| `src-rust/crates/query/src/agent_tool.rs` | 3 | Modify | Default to executor model/settings from managed config |
| `src-rust/crates/tools/src/lib.rs` | 3 | Modify | Add `managed_agent_config` to `ToolContext` |
| `src-rust/crates/tui/src/agents_view.rs` | 4 | Modify | Add `AgentRole`, role badges, cost breakdown display |
| `src-rust/crates/core/src/session_storage.rs` | 5 | Modify | Add `agent_role`, `managed_session_id` to `TranscriptMessage` |

---

## 8. Implementation Order and Timeline Estimate

```
Phase 1 (Config)          ████████░░░░░░░░░░░░  ~2 days
Phase 2 (Command)         ░░░░░░░░████████░░░░  ~2 days  (depends on Phase 1)
Phase 3 (Orchestrator)    ░░░░░░░░░░░░████████  ~3 days  (depends on Phase 1, 2)
Phase 4 (TUI)             ░░░░░░░░░░░░░░░░████  ~1 day   (depends on Phase 3)
Phase 5 (Sessions)        ░░░░░░░░░░░░░░░░████  ~1 day   (depends on Phase 3)
Phase 6 (Testing)         ░░░░░░░░████████████  ~ongoing, parallel with each phase
```

Phases 4 and 5 can run in parallel after Phase 3.

Total estimate: **7-9 working days** for a single developer.
