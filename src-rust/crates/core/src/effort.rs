// effort.rs — the single canonical EffortLevel enum and associated helpers.
//
// This is the one source of truth for effort across the whole workspace: the
// query loop (thinking budget + temperature), the provider request mapping
// (reasoning_effort), and the TUI (model picker / effort picker) all use this
// same enum. The TUI historically had a second `model_picker::EffortLevel`
// (Low/Normal/High/Max); that type is now a re-export of this one, with the old
// `Normal` mapped onto `Medium` (both `from_str("normal")` and the serde alias
// keep old configs working).
//
// The thinking-budget and temperature values for Low/Medium/High/Max are kept
// exactly as before because they are passed to the Anthropic API and must not
// change. `XHigh` slots between High and Max; `Ultracode` is the top level and
// pairs the model's maximum reasoning with the ultracode delegation/verification
// workflow (see `ULTRACODE_PROCEDURE`).

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// EffortLevel enum
// ---------------------------------------------------------------------------

/// The named effort levels supported by Claurst.
///
/// Ordered from least to most effort (the enum's *declaration order* is the
/// canonical ascending order — see the derived [`Ord`]). `None` and `Minimal`
/// are the two rungs below `Low`, ported from opencode's OpenAI reasoning ladder
/// (`reasoning_effort: "none" | "minimal"`); `Ultracode` is the top level: it
/// requests the model's maximum reasoning *and* activates the ultracode
/// operating procedure (bounded delegation across native primitives +
/// verification).
///
/// IMPORTANT: keep the variants in ascending order — [`Ord`]/[`PartialOrd`] are
/// derived from declaration order and the picker/effort ladders rely on it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum EffortLevel {
    /// No reasoning at all (opencode `reasoning_effort: "none"`). The model
    /// answers directly with thinking disabled.
    None,
    /// The smallest reasoning budget (opencode `reasoning_effort: "minimal"`).
    Minimal,
    /// Quick, straightforward implementation with minimal overhead.
    Low,
    /// Balanced approach with standard implementation and testing.
    ///
    /// This is the historical TUI `Normal` level; `"normal"` still deserializes
    /// and parses to `Medium` for backward compatibility.
    #[default]
    #[serde(alias = "normal")]
    Medium,
    /// Comprehensive implementation with extensive testing and documentation.
    High,
    /// Extended reasoning with a higher thinking budget for hard problems.
    #[serde(rename = "xhigh")]
    XHigh,
    /// Maximum capability with deepest reasoning.
    Max,
    /// Top reasoning plus the ultracode delegation & verification workflow.
    Ultracode,
}

impl EffortLevel {
    /// Parse an effort level from its string representation (case-insensitive).
    ///
    /// Accepts: `"none"`, `"minimal"`, `"low"`, `"medium"` (or `"normal"`),
    /// `"high"`, `"xhigh"`, `"max"`, `"ultracode"`. Returns `None` for any other
    /// value.
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" | "normal" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" | "x-high" | "extra-high" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            "ultracode" => Some(Self::Ultracode),
            _ => None,
        }
    }

    /// The lowercase string name of this effort level.
    ///
    /// Round-trips with `from_str`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Ultracode => "ultracode",
        }
    }

    /// Short human label used in the TUI (identical to [`as_str`]).
    ///
    /// [`as_str`]: Self::as_str
    pub fn label(&self) -> &'static str {
        self.as_str()
    }

    /// Whether this level is the top `Ultracode` level.
    pub fn is_ultracode(&self) -> bool {
        matches!(self, Self::Ultracode)
    }

    /// Return the extended-thinking budget in tokens for this effort level, or
    /// `None` if thinking should be disabled.
    ///
    /// Values (ascending):
    ///   None      → None  (reasoning disabled)
    ///   Minimal   → 1 024 (the smallest valid Anthropic thinking budget)
    ///   Low       → None  (no thinking)
    ///   Medium    → 5 000
    ///   High      → 10 000
    ///   XHigh     → 16 000
    ///   Max       → 20 000
    ///   Ultracode → 20 000 (the model's top reasoning budget)
    ///
    /// Low/Medium/High/Max are unchanged from the original mapping and must stay
    /// that way — they are sent verbatim to the Anthropic API. `None` maps to no
    /// thinking (opencode `reasoning_effort: "none"`); `Minimal` maps to the
    /// smallest budget Anthropic accepts (`1024`). Note these two rungs only ever
    /// appear on OpenAI-family ladders (where the effort *name* is sent, not the
    /// budget); no Anthropic model's ladder exposes them.
    pub fn thinking_budget_tokens(&self) -> Option<u32> {
        match self {
            Self::None | Self::Low => None,
            Self::Minimal => Some(1_024),
            Self::Medium => Some(5_000),
            Self::High => Some(10_000),
            Self::XHigh => Some(16_000),
            Self::Max | Self::Ultracode => Some(20_000),
        }
    }

    /// Return the temperature override for this effort level, or `None` to use
    /// the model's default.
    ///
    ///   Low → Some(0.0) — deterministic, cheap
    ///   everything else → None (model default)
    pub fn temperature(&self) -> Option<f32> {
        match self {
            Self::Low => Some(0.0),
            Self::None
            | Self::Minimal
            | Self::Medium
            | Self::High
            | Self::XHigh
            | Self::Max
            | Self::Ultracode => None,
        }
    }

    /// A single Unicode glyph used to represent this effort level.
    ///
    ///   None ∅  Minimal ◔  Low ○  Medium ◐  High ●  XHigh ◍  Max ◉  Ultracode ✦
    pub fn glyph(&self) -> &'static str {
        match self {
            Self::None => "∅",
            Self::Minimal => "◔",
            Self::Low => "○",
            Self::Medium => "◐",
            Self::High => "●",
            Self::XHigh => "◍",
            Self::Max => "◉",
            Self::Ultracode => "✦",
        }
    }

    /// A quarter-circle "fill" symbol used in the TUI pickers/status bar.
    ///
    ///   None ∅  Minimal ◔  Low ○  Medium ◐  High ◕  XHigh ●  Max ◉  Ultracode ✦
    pub fn symbol(&self) -> &'static str {
        match self {
            Self::None => "\u{2205}",      // ∅  empty set
            Self::Minimal => "\u{25d4}",   // ◔  quarter
            Self::Low => "\u{25cb}",       // ○  empty
            Self::Medium => "\u{25d0}",    // ◐  half
            Self::High => "\u{25d5}",      // ◕  three-quarter
            Self::XHigh => "\u{25cf}",     // ●  full
            Self::Max => "\u{25c9}",       // ◉  fisheye
            Self::Ultracode => "\u{2726}", // ✦  star
        }
    }

    /// Human-readable description of this effort level.
    pub fn description(&self) -> &'static str {
        match self {
            Self::None => "No reasoning — answer directly with thinking disabled",
            Self::Minimal => "The smallest reasoning budget for the quickest thinking",
            Self::Low => "Quick, straightforward implementation with minimal overhead",
            Self::Medium => "Balanced approach with standard implementation and testing",
            Self::High => {
                "Comprehensive implementation with extensive testing and documentation"
            }
            Self::XHigh => "Extended reasoning with a higher thinking budget for hard problems",
            Self::Max => "Maximum capability with the deepest reasoning",
            Self::Ultracode => {
                "Top reasoning plus the ultracode delegation & verification workflow"
            }
        }
    }

    /// Cycle to the next level in the legacy fixed Low↔Medium↔High↔Max cycle.
    ///
    /// Historically drove the /model picker's ←/→ effort selector; that selector
    /// now cycles the model's actual variants ladder (see
    /// `ModelPickerState::effort_next`). These helpers are retained for the
    /// effort-picker tests and any caller that still wants the fixed cycle. `Max`
    /// is only reached when the model supports it (`supports_max`). The
    /// out-of-cycle rungs (`None`/`Minimal`/`XHigh`/`Ultracode`) fall back to the
    /// nearest in-cycle level.
    pub fn next(self, supports_max: bool) -> Self {
        match self {
            Self::None => Self::Minimal,
            Self::Minimal => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => {
                if supports_max {
                    Self::Max
                } else {
                    Self::Low
                }
            }
            Self::XHigh | Self::Max | Self::Ultracode => Self::Low,
        }
    }

    /// Cycle to the previous level (inverse of [`next`]).
    ///
    /// [`next`]: Self::next
    pub fn prev(self, supports_max: bool) -> Self {
        match self {
            Self::None | Self::Minimal => Self::Low,
            Self::Low => {
                if supports_max {
                    Self::Max
                } else {
                    Self::High
                }
            }
            Self::Medium => Self::Low,
            Self::High => Self::Medium,
            Self::XHigh => Self::High,
            Self::Max => Self::High,
            Self::Ultracode => Self::Max,
        }
    }
}

impl std::fmt::Display for EffortLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Ultracode: keyword detection + operating procedure (single source of truth)
// ---------------------------------------------------------------------------

/// The single keyword that activates ultracode mode for a turn.
///
/// Historically an "ultra code" (two-word) alias also matched; that has been
/// dropped in favour of the single word `ultracode`.
pub const ULTRACODE_KEYWORD: &str = "ultracode";

/// The ultracode operating procedure.
///
/// This is the *single source of truth* for ultracode's behaviour. When the
/// effective effort for a turn is [`EffortLevel::Ultracode`], this text is
/// injected as a per-turn system-prompt addendum by the query loop (see
/// [`ultracode_system_prompt_addendum`]).
pub const ULTRACODE_PROCEDURE: &str = r#"# Ultracode

You are operating in **ultracode** mode: a disciplined, supervised workflow for
serious coding work. Plan, split, delegate when it genuinely helps, integrate in
the parent session, and verify. Use the smallest workflow that can prove the
result -- do not manufacture ceremony for small tasks.

## Contract

- Ultracode is a claurst operating procedure, not a separate runtime. System,
  developer, and user rules always win over this text.
- Do NOT commit, push, publish, deploy, or delete anything unless the user
  explicitly asks. Ask one clear approval question before any destructive or
  irreversible action (mass rename/delete, force-push, migrations, production
  data, credentials/secrets/billing, broad codemods, or large agent fan-out).
  If approval is missing, continue only with safe read-only or draft work.
- Never use delegation to avoid understanding the integration path. The parent
  session owns integration, verification, and the final answer.

## 1. Classify the task

Before acting, state briefly:

- **type**: research | code change | bug fix | migration | audit | docs | design | QA | release
- **risk**: low | medium | high
- **blast radius**: single file | module | repo-wide | external system
- **verification**: none | command | tests | build | manual checklist
- **delegation**: useful | not useful (do bounded, independent packets exist?)

Then pick ONE mode.

## 2. Pick a mode

### Direct
Small, clear, tightly-coupled tasks with no useful independent packets (one file,
one command, a small function, a typo). Just do it, then run the narrowest useful
check. No artifacts.

### Workflow
Multi-phase or risky work that benefits from separated packets, but delegation is
not useful or not available. Keep a concrete plan (goal, success criteria,
constraints, risk, packets, verification), execute packets as isolated passes in
this session, integrate, then verify. Write scratch notes under
`.workflow/ultracode/<slug>/` only when they reduce risk.

### Delegated (default for non-trivial ultracode work)
When the work has bounded, non-overlapping, independent packets and delegation is
allowed, use claurst's native delegation primitives. Keep the blocking critical
path in the parent; delegate only useful sidecar work.

## 3. Delegated mode -- claurst native primitives

- **`Agent`** -- spawn a subagent for one bounded packet (read-only exploration,
  test writing, triage, or a disjoint write scope). Use `isolation: "worktree"`
  for write-heavy packets that must not collide, and `run_in_background: true` to
  fan several out at once, then integrate their final messages in the parent.
- **`TeamCreate`** (+ `TeamDelete`) -- stand up a named swarm/coordinator when
  several agents should work a shared task in parallel with restricted tool lists
  and aggregated output. Good for parallel audits or multi-track implementation.
- **`TaskCreate`** (+ `TaskGet` / `TaskUpdate` / `TaskList` / `TaskStop`) --
  track and run background work items; poll or stop them from the parent.

Rules:

- Plan first, then split into bounded, non-overlapping packets before spawning.
- Default to **2-4** subagents for useful independent work; do not exceed **~5**
  total without explicit user approval. Run at most one broad implementation wave
  and one review/verification wave unless the user approves more.
- Prefer delegation for read-heavy exploration, test writing, triage, and
  summarization. Use write-capable agents only when file ownership is disjoint.
- Tell every write-capable agent: "You are not alone in the codebase. Do not
  revert edits made by others; adapt to nearby changes." Give each a concrete
  objective, explicit ownership, and an expected-output shape.
- Only wait on an agent when its result blocks the next parent step.
- If native agents are unavailable or not useful, fall back to Workflow mode and
  say so briefly with the concrete reason.

## 4. Integrate (parent-owned)

Read each result, check claimed edits against the actual files, resolve
disagreements with source/tests/docs, and reject outputs that lack evidence.
Never paste raw agent logs as the final answer.

## 5. Verify (scaled by risk)

- **low**: inspect the diff + a targeted test.
- **medium**: targeted tests + typecheck/lint + affected build.
- **high**: full tests (if practical) + build + smoke + an independent review pass.

Mark each check pass | fail | skipped (with reason). Report skipped checks
honestly. Finish with a short summary: outcome, key files changed, verification
run, and remaining risk.

## Composing with /goal (multi-turn)

Ultracode is compatible with claurst's continuation/goal mode: a large task can
span multiple turns. For a long, autonomous objective, combine ultracode with
`/goal <objective>` -- the goal loop keeps working across turns while ultracode
governs *how* each turn plans, delegates, integrates, and verifies. Ultracode
mode itself is scoped to the current turn; re-invoke it (or run it under a goal)
for sustained multi-turn work.

## Your task

The task is the user's latest message in this conversation."#;

/// Find every whole-word, case-insensitive occurrence of the `ultracode`
/// keyword in `text`, returned as non-overlapping `(start, end)` byte ranges.
///
/// Thin wrapper over the generalised [`crate::keywords::keyword_match_ranges`]
/// matcher (this is the caller that inline keywords generalised). "Whole-word"
/// means the byte immediately before/after a match must not be ASCII
/// alphanumeric (so `ultracoder` does not match).
pub fn ultracode_match_ranges(text: &str) -> Vec<(usize, usize)> {
    crate::keywords::keyword_match_ranges(text, ULTRACODE_KEYWORD)
}

/// Whether `text` contains the `ultracode` keyword (whole-word, case-insensitive).
pub fn text_triggers_ultracode(text: &str) -> bool {
    !ultracode_match_ranges(text).is_empty()
}

/// Build the per-turn system-prompt addendum for ultracode mode.
///
/// Wraps [`ULTRACODE_PROCEDURE`] with a short activation framing. This is the
/// same text that the query loop injects whenever the effective effort for a
/// turn is [`EffortLevel::Ultracode`].
pub fn ultracode_system_prompt_addendum() -> String {
    format!(
        "\n## Ultracode Mode\n\
         You are operating at **ultracode** effort for this turn: use the model's \
         maximum reasoning and follow the operating procedure below.\n\n{}\n",
        ULTRACODE_PROCEDURE
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Every effort level in canonical ascending order — the single list the
    /// tests iterate so a new rung is covered everywhere at once.
    const ALL_LEVELS: [EffortLevel; 8] = [
        EffortLevel::None,
        EffortLevel::Minimal,
        EffortLevel::Low,
        EffortLevel::Medium,
        EffortLevel::High,
        EffortLevel::XHigh,
        EffortLevel::Max,
        EffortLevel::Ultracode,
    ];

    #[test]
    fn from_str_roundtrips() {
        for level in ALL_LEVELS {
            let parsed = EffortLevel::from_str(level.as_str());
            assert_eq!(parsed, Some(level), "from_str({:?}) should round-trip", level);
        }
    }

    #[test]
    fn declaration_order_is_ascending() {
        // Ord is derived from declaration order; the ladders rely on it.
        for pair in ALL_LEVELS.windows(2) {
            assert!(pair[0] < pair[1], "{:?} must rank below {:?}", pair[0], pair[1]);
        }
        assert_eq!(*ALL_LEVELS.iter().min().unwrap(), EffortLevel::None);
        assert_eq!(*ALL_LEVELS.iter().max().unwrap(), EffortLevel::Ultracode);
    }

    #[test]
    fn from_str_case_insensitive_and_aliases() {
        assert_eq!(EffortLevel::from_str("none"), Some(EffortLevel::None));
        assert_eq!(EffortLevel::from_str("NONE"), Some(EffortLevel::None));
        assert_eq!(EffortLevel::from_str("Minimal"), Some(EffortLevel::Minimal));
        assert_eq!(EffortLevel::from_str("LOW"), Some(EffortLevel::Low));
        assert_eq!(EffortLevel::from_str("Medium"), Some(EffortLevel::Medium));
        // "normal" is the legacy TUI spelling for Medium.
        assert_eq!(EffortLevel::from_str("normal"), Some(EffortLevel::Medium));
        assert_eq!(EffortLevel::from_str("NORMAL"), Some(EffortLevel::Medium));
        assert_eq!(EffortLevel::from_str("HIGH"), Some(EffortLevel::High));
        assert_eq!(EffortLevel::from_str("XHigh"), Some(EffortLevel::XHigh));
        assert_eq!(EffortLevel::from_str("Max"), Some(EffortLevel::Max));
        assert_eq!(
            EffortLevel::from_str("ULTRACODE"),
            Some(EffortLevel::Ultracode)
        );
    }

    #[test]
    fn from_str_unknown_returns_none() {
        assert_eq!(EffortLevel::from_str("ultra"), None);
        assert_eq!(EffortLevel::from_str(""), None);
        assert_eq!(EffortLevel::from_str("3"), None);
    }

    #[test]
    fn thinking_budget_is_ascending_and_preserves_legacy() {
        // Legacy values unchanged.
        assert_eq!(EffortLevel::Low.thinking_budget_tokens(), None);
        assert_eq!(EffortLevel::Medium.thinking_budget_tokens(), Some(5_000));
        assert_eq!(EffortLevel::High.thinking_budget_tokens(), Some(10_000));
        assert_eq!(EffortLevel::Max.thinking_budget_tokens(), Some(20_000));
        // XHigh slots between High and Max; Ultracode = top reasoning.
        assert_eq!(EffortLevel::XHigh.thinking_budget_tokens(), Some(16_000));
        assert_eq!(EffortLevel::Ultracode.thinking_budget_tokens(), Some(20_000));
        // New rungs: None disables thinking, Minimal is the smallest budget.
        assert_eq!(EffortLevel::None.thinking_budget_tokens(), None);
        assert_eq!(EffortLevel::Minimal.thinking_budget_tokens(), Some(1_024));
    }

    #[test]
    fn temperature_matches_legacy() {
        assert_eq!(EffortLevel::Low.temperature(), Some(0.0));
        for level in [
            EffortLevel::None,
            EffortLevel::Minimal,
            EffortLevel::Medium,
            EffortLevel::High,
            EffortLevel::XHigh,
            EffortLevel::Max,
            EffortLevel::Ultracode,
        ] {
            assert_eq!(level.temperature(), None, "{level:?} temp should be default");
        }
    }

    #[test]
    fn glyph_and_symbol_are_distinct_per_variant() {
        let levels = ALL_LEVELS;
        let glyphs: std::collections::HashSet<_> = levels.iter().map(|l| l.glyph()).collect();
        let symbols: std::collections::HashSet<_> = levels.iter().map(|l| l.symbol()).collect();
        assert_eq!(glyphs.len(), levels.len(), "glyphs must be unique");
        assert_eq!(symbols.len(), levels.len(), "symbols must be unique");
        // Legacy glyphs preserved.
        assert_eq!(EffortLevel::Low.glyph(), "○");
        assert_eq!(EffortLevel::Medium.glyph(), "◐");
        assert_eq!(EffortLevel::High.glyph(), "●");
        assert_eq!(EffortLevel::Max.glyph(), "◉");
    }

    #[test]
    fn default_is_medium() {
        assert_eq!(EffortLevel::default(), EffortLevel::Medium);
    }

    #[test]
    fn is_ultracode_only_for_top() {
        assert!(EffortLevel::Ultracode.is_ultracode());
        for level in [
            EffortLevel::None,
            EffortLevel::Minimal,
            EffortLevel::Low,
            EffortLevel::Medium,
            EffortLevel::High,
            EffortLevel::XHigh,
            EffortLevel::Max,
        ] {
            assert!(!level.is_ultracode());
        }
    }

    #[test]
    fn cycle_preserves_legacy_low_med_high_max() {
        // Non-max model: Low -> Medium -> High -> Low.
        assert_eq!(EffortLevel::Low.next(false), EffortLevel::Medium);
        assert_eq!(EffortLevel::Medium.next(false), EffortLevel::High);
        assert_eq!(EffortLevel::High.next(false), EffortLevel::Low);
        // Max-capable model: High -> Max -> Low.
        assert_eq!(EffortLevel::High.next(true), EffortLevel::Max);
        assert_eq!(EffortLevel::Max.next(true), EffortLevel::Low);
        // prev is the inverse.
        assert_eq!(EffortLevel::Low.prev(true), EffortLevel::Max);
        assert_eq!(EffortLevel::Max.prev(true), EffortLevel::High);
        assert_eq!(EffortLevel::Low.prev(false), EffortLevel::High);
    }

    #[test]
    fn serde_roundtrips_lowercase_with_normal_alias() {
        assert_eq!(
            serde_json::to_string(&EffortLevel::XHigh).unwrap(),
            "\"xhigh\""
        );
        assert_eq!(
            serde_json::to_string(&EffortLevel::Ultracode).unwrap(),
            "\"ultracode\""
        );
        // Legacy "normal" deserializes to Medium.
        let m: EffortLevel = serde_json::from_str("\"normal\"").unwrap();
        assert_eq!(m, EffortLevel::Medium);
    }

    #[test]
    fn display_matches_as_str() {
        for level in ALL_LEVELS {
            assert_eq!(format!("{}", level), level.as_str());
        }
    }

    #[test]
    fn serde_roundtrips_none_and_minimal() {
        assert_eq!(serde_json::to_string(&EffortLevel::None).unwrap(), "\"none\"");
        assert_eq!(
            serde_json::to_string(&EffortLevel::Minimal).unwrap(),
            "\"minimal\""
        );
        let n: EffortLevel = serde_json::from_str("\"none\"").unwrap();
        assert_eq!(n, EffortLevel::None);
        let m: EffortLevel = serde_json::from_str("\"minimal\"").unwrap();
        assert_eq!(m, EffortLevel::Minimal);
    }

    // ---- ultracode keyword + procedure ----------------------------------

    #[test]
    fn ultracode_match_ranges_finds_single_word() {
        let text = "please ultracode this";
        let r = ultracode_match_ranges(text);
        assert_eq!(r.len(), 1);
        let (s, e) = r[0];
        assert_eq!(&text[s..e], "ultracode");
    }

    #[test]
    fn ultracode_match_ranges_case_insensitive() {
        assert_eq!(ultracode_match_ranges("UltraCode now").len(), 1);
        assert!(text_triggers_ultracode("(ULTRACODE)"));
        assert!(text_triggers_ultracode("ultracode."));
    }

    #[test]
    fn ultracode_two_word_alias_no_longer_matches() {
        // The old "ultra code" spelling is intentionally dropped.
        assert!(ultracode_match_ranges("do ultra code it").is_empty());
        assert!(!text_triggers_ultracode("please ultra code this"));
    }

    #[test]
    fn ultracode_respects_word_boundaries() {
        assert!(ultracode_match_ranges("ultracoder").is_empty());
        assert!(ultracode_match_ranges("supraultracode1").is_empty());
        assert!(!text_triggers_ultracode("this is a normal prompt"));
    }

    #[test]
    fn ultracode_procedure_names_native_primitives() {
        for needle in ["Agent", "TeamCreate", "TaskCreate", "/goal", "Delegated"] {
            assert!(
                ULTRACODE_PROCEDURE.contains(needle),
                "ultracode procedure missing `{needle}`"
            );
        }
    }

    #[test]
    fn ultracode_addendum_wraps_procedure() {
        let add = ultracode_system_prompt_addendum();
        assert!(add.contains("Ultracode Mode"));
        assert!(add.contains("## Your task"));
        assert!(add.contains("Agent"));
        assert!(add.contains("TeamCreate"));
        assert!(!add.contains("$ARGUMENTS"), "no template placeholder should remain");
    }
}
