//! OSC 7501 Program Status Protocol reporting.
//!
//! Tells the hosting terminal what the interactive TUI is doing (idle,
//! working, blocked on the user, done, failed) so tab bars and session pickers
//! (Rex, cmux, ...) can show which session needs attention without the user
//! opening every tab. Only the root record is used.
//!
//! Spec: <https://www.superlogical.com/rex/docs/build/program-status>

use base64::Engine;
use claurst_query::QueryOutcome;

use crate::app::App;

/// Stable program identifier sent as the `app` key on every report. Each report
/// replaces the record completely, so it is repeated every time.
const APP_ID: &str = "claurst";

/// Removes every OSC 7501 record this terminal holds for us. Sent on exit.
pub const CLEAR_SEQUENCE: &str = "\x1b]7501;state=clear\x07";

/// What the user has to do before a blocked program can continue (`kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockedKind {
    /// Approval to do something (tool permission, MCP server approval).
    Permission,
    /// The user must answer a question (AskUserQuestion, MCP elicitation).
    Question,
    /// A login, token or credential (device / browser auth flow).
    Auth,
}

impl BlockedKind {
    fn as_str(self) -> &'static str {
        match self {
            BlockedKind::Permission => "permission",
            BlockedKind::Question => "question",
            BlockedKind::Auth => "auth",
        }
    }

    pub(crate) fn message(self) -> &'static str {
        match self {
            BlockedKind::Permission => "Permission required",
            BlockedKind::Question => "Question needs your answer",
            BlockedKind::Auth => "Sign-in required",
        }
    }
}

/// How the most recent turn ended, kept until the user starts a new prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    Done,
    Error,
}

impl TurnOutcome {
    /// Map a finished query to the outcome to report. A user cancellation maps
    /// to `None` (reported as `idle`, per the spec's interruption rule); a
    /// panicked or aborted task (`Err`) counts as an error.
    pub fn from_query_result<E>(result: &Result<QueryOutcome, E>) -> Option<Self> {
        match result {
            Ok(QueryOutcome::Cancelled) => None,
            Ok(QueryOutcome::Error(_)) | Err(_) => Some(TurnOutcome::Error),
            Ok(QueryOutcome::EndTurn { .. })
            | Ok(QueryOutcome::MaxTokens { .. })
            | Ok(QueryOutcome::BudgetExceeded { .. }) => Some(TurnOutcome::Done),
        }
    }
}

/// One OSC 7501 report for the root record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramStatus {
    Idle,
    Working,
    Blocked(BlockedKind),
    Done,
    Error,
}

impl ProgramStatus {
    fn state(self) -> &'static str {
        match self {
            ProgramStatus::Idle => "idle",
            ProgramStatus::Working => "working",
            ProgramStatus::Blocked(_) => "blocked",
            ProgramStatus::Done => "done",
            ProgramStatus::Error => "error",
        }
    }

    /// Non-zero code identifying this status, used to dedupe emissions
    /// (`0` is reserved for "nothing emitted").
    pub(crate) fn code(self) -> u8 {
        match self {
            ProgramStatus::Idle => 1,
            ProgramStatus::Working => 2,
            ProgramStatus::Done => 3,
            ProgramStatus::Error => 4,
            ProgramStatus::Blocked(BlockedKind::Permission) => 5,
            ProgramStatus::Blocked(BlockedKind::Question) => 6,
            ProgramStatus::Blocked(BlockedKind::Auth) => 7,
        }
    }

    /// The full escape sequence: `ESC ] 7501 ; pairs BEL`. BEL termination
    /// matches the OSC 9;4 progress sequence; the spec accepts BEL or ST.
    /// `msg` is standard base64 of single-line UTF-8, as the spec requires.
    pub fn osc_sequence(self) -> String {
        let mut seq = format!("\x1b]7501;state={}", self.state());
        if let ProgramStatus::Blocked(kind) = self {
            seq.push_str(":kind=");
            seq.push_str(kind.as_str());
        }
        seq.push_str(":app=");
        seq.push_str(APP_ID);
        if let ProgramStatus::Blocked(kind) = self {
            seq.push_str(":msg=");
            seq.push_str(&base64::engine::general_purpose::STANDARD.encode(kind.message()));
        }
        seq.push('\x07');
        seq
    }
}

impl App {
    /// The program status to report for the current frame.
    ///
    /// Priority: a dialog waiting on the user (`blocked`) beats an in-flight
    /// turn (`working`), which beats the last turn's outcome (`done` /
    /// `error`). `query_in_flight` covers the gaps between model turns while
    /// tools run, when `is_streaming` briefly drops. The interactive loop
    /// clears `last_turn_outcome` on the first key press after a turn ends,
    /// which turns `done` / `error` back into `idle`.
    pub fn program_status(&self, query_in_flight: bool) -> ProgramStatus {
        if self.permission_request.is_some() || self.mcp_approval.visible {
            return ProgramStatus::Blocked(BlockedKind::Permission);
        }
        if self.ask_user_dialog.visible || self.elicitation.visible {
            return ProgramStatus::Blocked(BlockedKind::Question);
        }
        if self.device_auth_dialog.visible {
            return ProgramStatus::Blocked(BlockedKind::Auth);
        }
        if self.is_streaming || query_in_flight {
            return ProgramStatus::Working;
        }
        match self.last_turn_outcome {
            Some(TurnOutcome::Done) => ProgramStatus::Done,
            Some(TurnOutcome::Error) => ProgramStatus::Error,
            None => ProgramStatus::Idle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claurst_core::config::Config;
    use claurst_core::cost::CostTracker;

    fn make_app() -> App {
        App::new(Config::default(), CostTracker::new())
    }

    #[test]
    fn sequences_match_spec_bytes() {
        assert_eq!(ProgramStatus::Idle.osc_sequence(), "\x1b]7501;state=idle:app=claurst\x07");
        assert_eq!(
            ProgramStatus::Working.osc_sequence(),
            "\x1b]7501;state=working:app=claurst\x07"
        );
        assert_eq!(ProgramStatus::Done.osc_sequence(), "\x1b]7501;state=done:app=claurst\x07");
        assert_eq!(ProgramStatus::Error.osc_sequence(), "\x1b]7501;state=error:app=claurst\x07");
        assert_eq!(CLEAR_SEQUENCE, "\x1b]7501;state=clear\x07");
    }

    #[test]
    fn blocked_sequences_carry_kind_and_base64_msg() {
        assert_eq!(
            ProgramStatus::Blocked(BlockedKind::Permission).osc_sequence(),
            "\x1b]7501;state=blocked:kind=permission:app=claurst:msg=UGVybWlzc2lvbiByZXF1aXJlZA==\x07"
        );
        assert_eq!(
            ProgramStatus::Blocked(BlockedKind::Question).osc_sequence(),
            "\x1b]7501;state=blocked:kind=question:app=claurst:msg=UXVlc3Rpb24gbmVlZHMgeW91ciBhbnN3ZXI=\x07"
        );
        assert_eq!(
            ProgramStatus::Blocked(BlockedKind::Auth).osc_sequence(),
            "\x1b]7501;state=blocked:kind=auth:app=claurst:msg=U2lnbi1pbiByZXF1aXJlZA==\x07"
        );
    }

    #[test]
    fn sequences_stay_within_spec_grammar() {
        let all = [
            ProgramStatus::Idle,
            ProgramStatus::Working,
            ProgramStatus::Done,
            ProgramStatus::Error,
            ProgramStatus::Blocked(BlockedKind::Permission),
            ProgramStatus::Blocked(BlockedKind::Question),
            ProgramStatus::Blocked(BlockedKind::Auth),
        ];
        let mut codes = std::collections::HashSet::new();
        for status in all {
            let seq = status.osc_sequence();
            assert!(seq.len() <= 4096);
            let body = seq
                .strip_prefix("\x1b]7501;")
                .and_then(|s| s.strip_suffix('\x07'))
                .unwrap();
            for pair in body.split(':') {
                let (key, value) = pair.split_once('=').unwrap();
                assert!(!key.is_empty() && key.len() <= 16);
                assert!(key.bytes().all(|b| b.is_ascii_lowercase()));
                assert!(value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.,+/=-".contains(&b)));
            }
            assert!(status.code() != 0, "0 is reserved for nothing emitted");
            assert!(codes.insert(status.code()), "codes must be unique");
        }
    }

    #[test]
    fn fresh_app_is_idle() {
        let app = make_app();
        assert_eq!(app.program_status(false), ProgramStatus::Idle);
    }

    #[test]
    fn streaming_or_query_in_flight_is_working() {
        let mut app = make_app();
        app.is_streaming = true;
        assert_eq!(app.program_status(false), ProgramStatus::Working);
        app.is_streaming = false;
        assert_eq!(app.program_status(true), ProgramStatus::Working);
    }

    #[test]
    fn dialogs_waiting_on_user_are_blocked_even_mid_turn() {
        let mut app = make_app();
        app.is_streaming = true;
        app.permission_request = Some(crate::dialogs::PermissionRequest::standard(
            "t1".to_string(),
            "Bash".to_string(),
            "run ls".to_string(),
        ));
        assert_eq!(app.program_status(true), ProgramStatus::Blocked(BlockedKind::Permission));
        app.permission_request = None;

        app.mcp_approval.visible = true;
        assert_eq!(app.program_status(false), ProgramStatus::Blocked(BlockedKind::Permission));
        app.mcp_approval.visible = false;

        app.ask_user_dialog.visible = true;
        assert_eq!(app.program_status(true), ProgramStatus::Blocked(BlockedKind::Question));
        app.ask_user_dialog.visible = false;

        app.elicitation.visible = true;
        assert_eq!(app.program_status(true), ProgramStatus::Blocked(BlockedKind::Question));
        app.elicitation.visible = false;

        app.device_auth_dialog.visible = true;
        assert_eq!(app.program_status(false), ProgramStatus::Blocked(BlockedKind::Auth));
        app.device_auth_dialog.visible = false;

        assert_eq!(app.program_status(true), ProgramStatus::Working);
    }

    #[test]
    fn finished_turn_reports_outcome_until_acknowledged() {
        let mut app = make_app();
        app.last_turn_outcome = Some(TurnOutcome::Done);
        assert_eq!(app.program_status(false), ProgramStatus::Done);
        // A draft typed during the turn does not hide the result.
        app.prompt_input.text = "follow-up".to_string();
        assert_eq!(app.program_status(false), ProgramStatus::Done);

        // The interactive loop clears the outcome on the next key press.
        app.last_turn_outcome = None;
        assert_eq!(app.program_status(false), ProgramStatus::Idle);

        app.last_turn_outcome = Some(TurnOutcome::Error);
        assert_eq!(app.program_status(false), ProgramStatus::Error);
    }

    #[test]
    fn query_results_map_to_outcomes() {
        type R = Result<QueryOutcome, ()>;
        let cancelled: R = Ok(QueryOutcome::Cancelled);
        assert_eq!(TurnOutcome::from_query_result(&cancelled), None);
        let failed: R = Ok(QueryOutcome::Error(claurst_core::error::ClaudeError::Other(
            "boom".to_string(),
        )));
        assert_eq!(TurnOutcome::from_query_result(&failed), Some(TurnOutcome::Error));
        let panicked: R = Err(());
        assert_eq!(TurnOutcome::from_query_result(&panicked), Some(TurnOutcome::Error));
        let budget: R = Ok(QueryOutcome::BudgetExceeded { cost_usd: 1.0, limit_usd: 0.5 });
        assert_eq!(TurnOutcome::from_query_result(&budget), Some(TurnOutcome::Done));
    }
}
