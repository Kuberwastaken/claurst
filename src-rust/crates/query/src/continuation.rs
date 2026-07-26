// continuation.rs — In-loop continuation policy for `run_query_loop`.
//
// At the end of every turn that finishes with `end_turn` (no tool calls),
// `run_query_loop` consults a `ContinuationPolicy` to decide whether to keep
// going — and if so, with what follow-up user message — instead of always
// returning after one turn.
//
// This mirrors pi's agent-loop callbacks (`shouldStopAfterTurn`,
// `getFollowUpMessages`, `prepareNextTurn`) and its `agentLoopContinue`
// primitive for "keep going without a new user message". The decision now
// lives INSIDE the loop, so autonomous continuation (e.g. `/goal`) no longer
// requires the CLI REPL to re-dispatch a fresh turn after the loop returns.
//
// The default policy is `StopPolicy`: stop after one turn, exactly reproducing
// the historical non-goal behaviour. Goal-driven continuation is provided by
// `GoalPolicy` (see the goal-policy section below), which reuses the existing
// `goal_loop` guards (runaway cap, soft token budget, continuation message).

/// Inputs available to a continuation policy after a turn completes with
/// `end_turn` (no tool calls were requested).
pub struct TurnEndContext<'a> {
    /// Session identifier — used to look up any active goal for this session.
    pub session_id: &'a str,
    /// Cumulative token count for the whole session (soft-budget accounting).
    pub total_tokens_used: u64,
    /// Wall-clock seconds this turn took (goal time accounting).
    pub turn_elapsed_secs: u64,
}

/// Decision returned by a continuation policy at the end of a completed turn.
#[derive(Debug, Clone)]
pub enum ContinuationDecision {
    /// Inject `message` as the next user turn and keep the loop running.
    Continue { message: String },
    /// Stop the loop. `note`, when present, is surfaced to the user as a
    /// status line (e.g. the goal's paused / budget-limited message).
    Stop { note: Option<String> },
}

impl ContinuationDecision {
    /// Whether this decision keeps the loop running.
    pub fn is_continue(&self) -> bool {
        matches!(self, ContinuationDecision::Continue { .. })
    }
}

/// A policy the runner consults at the end of each completed `end_turn` turn.
///
/// Implementations must be cheap and side-effect-aware: `decide` is called at
/// most once per turn, from the async loop, but must never hold a lock across
/// an `.await` (it is fully synchronous by design).
pub trait ContinuationPolicy: Send + Sync {
    fn decide(&self, ctx: &TurnEndContext<'_>) -> ContinuationDecision;
}

/// Default policy: always stop after the turn completes.
///
/// This is the historical, non-goal behaviour — a normal turn that ends with
/// `end_turn` returns immediately instead of continuing.
#[derive(Debug, Clone, Copy, Default)]
pub struct StopPolicy;

impl ContinuationPolicy for StopPolicy {
    fn decide(&self, _ctx: &TurnEndContext<'_>) -> ContinuationDecision {
        ContinuationDecision::Stop { note: None }
    }
}

/// Goal-driven continuation policy (the `/goal` feature).
///
/// Reuses the existing `goal_loop` guards verbatim — the runaway turn cap, the
/// soft token budget, and the per-turn continuation message. While the session
/// has an active goal and its guards allow, the loop continues with the goal
/// continuation message injected as the next user turn; otherwise it stops and
/// surfaces the same paused / budget-limited / runaway note as before.
///
/// This policy relocates only WHERE the decision is made (in-loop, per turn),
/// not the guards themselves: it delegates to
/// [`crate::goal_loop::check_and_continue_goal`], which opens the default goal
/// store and applies the identical logic the CLI post-loop path used to run.
#[derive(Debug, Clone, Copy, Default)]
pub struct GoalPolicy;

impl ContinuationPolicy for GoalPolicy {
    fn decide(&self, ctx: &TurnEndContext<'_>) -> ContinuationDecision {
        use crate::goal_loop::GoalContinuation;
        match crate::goal_loop::check_and_continue_goal(
            ctx.session_id,
            ctx.total_tokens_used,
            ctx.turn_elapsed_secs,
        ) {
            GoalContinuation::Continue { message } => ContinuationDecision::Continue { message },
            // Paused / budget / runaway / complete: stop, surfacing the same
            // user-facing note the CLI used to print.
            GoalContinuation::Stop { reason } => ContinuationDecision::Stop {
                note: reason.user_message(),
            },
            // No goal set for this session: behave exactly like `StopPolicy`.
            GoalContinuation::NoGoal => ContinuationDecision::Stop { note: None },
        }
    }
}

/// Selects which continuation policy `run_query_loop` uses for a run.
///
/// Stored on `QueryConfig` so callers opt in per invocation. Subagents,
/// headless runs, and every non-goal interactive turn use `Default`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContinuationMode {
    /// Stop after the turn completes (default, non-goal behaviour).
    #[default]
    Default,
    /// Goal-driven autonomous continuation (the `/goal` feature).
    Goal,
}

impl ContinuationMode {
    /// Build the concrete policy for this mode.
    pub fn policy(self) -> Box<dyn ContinuationPolicy> {
        match self {
            ContinuationMode::Default => Box::new(StopPolicy),
            ContinuationMode::Goal => Box::new(GoalPolicy),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> TurnEndContext<'static> {
        TurnEndContext {
            session_id: "sess",
            total_tokens_used: 0,
            turn_elapsed_secs: 0,
        }
    }

    #[test]
    fn stop_policy_always_stops() {
        let decision = StopPolicy.decide(&ctx());
        assert!(!decision.is_continue());
        match decision {
            ContinuationDecision::Stop { note } => assert!(note.is_none()),
            _ => panic!("StopPolicy must stop with no note"),
        }
    }

    #[test]
    fn default_mode_resolves_to_stop() {
        let policy = ContinuationMode::default().policy();
        assert!(!policy.decide(&ctx()).is_continue());
    }
}
