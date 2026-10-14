//! Attention alerts (#444): a terminal bell plus a desktop notification when
//! a turn finishes or blocks on the user while the terminal is in the
//! background.
//!
//! Desktop notifications use OSC 9 on terminals known to show it (iTerm2,
//! WezTerm, Ghostty, kitty) and OSC 777 elsewhere; terminals that support
//! neither ignore the sequence and still ring the bell. Inside tmux the OSC
//! is wrapped in a DCS passthrough (shown when `allow-passthrough` is on).
//! Gated on the `notifications` setting and on crossterm focus events.

use crate::app::App;
use crate::program_status::ProgramStatus;

/// Why the user is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attention {
    /// The turn finished and the prompt is waiting.
    Done,
    /// The turn ended in an error.
    Failed,
    /// A dialog waits on the user (permission, question, sign-in).
    Blocked(crate::program_status::BlockedKind),
}

impl Attention {
    /// The alert for a change of program status, if the new status needs
    /// the user. Repeating a status never alerts again.
    pub fn for_transition(previous: ProgramStatus, next: ProgramStatus) -> Option<Self> {
        if previous == next {
            return None;
        }
        match next {
            ProgramStatus::Done => Some(Attention::Done),
            ProgramStatus::Error => Some(Attention::Failed),
            ProgramStatus::Blocked(kind) => Some(Attention::Blocked(kind)),
            ProgramStatus::Idle | ProgramStatus::Working => None,
        }
    }

    fn message(self) -> &'static str {
        match self {
            Attention::Done => "Turn finished",
            Attention::Failed => "Turn failed",
            Attention::Blocked(kind) => kind.message(),
        }
    }
}

/// Desktop-notification escape understood by the hosting terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopProtocol {
    /// `OSC 9 ; message BEL` (iTerm2, WezTerm, Ghostty, kitty).
    Osc9,
    /// `OSC 777 ; notify ; title ; body BEL` (foot, urxvt, VTE builds that
    /// carry the notify patch, …).
    Osc777,
}

/// Pick the notification escape from `TERM_PROGRAM` and `TERM`.
pub fn desktop_protocol(term_program: &str, term: &str) -> DesktopProtocol {
    match term_program {
        "iTerm.app" | "WezTerm" | "ghostty" => DesktopProtocol::Osc9,
        _ if term == "xterm-kitty" || term == "xterm-ghostty" => DesktopProtocol::Osc9,
        _ => DesktopProtocol::Osc777,
    }
}

/// The bytes to write for `attention`: the desktop notification (wrapped for
/// tmux when `in_tmux`) followed by a bell.
pub fn alert_sequence(attention: Attention, protocol: DesktopProtocol, in_tmux: bool) -> String {
    let body = attention.message();
    let osc = match protocol {
        DesktopProtocol::Osc9 => format!("\x1b]9;Claurst: {body}\x07"),
        DesktopProtocol::Osc777 => format!("\x1b]777;notify;Claurst;{body}\x07"),
    };
    let osc = if in_tmux {
        format!("\x1bPtmux;{}\x1b\\", osc.replace('\x1b', "\x1b\x1b"))
    } else {
        osc
    };
    format!("{osc}\x07")
}

/// Write the alert for `attention` to the terminal. No-op when stdout is
/// not a terminal or `TERM=dumb`.
pub fn emit(attention: Attention) {
    use std::io::{IsTerminal, Write};
    let mut out = std::io::stdout();
    let term = std::env::var("TERM").unwrap_or_default();
    if !out.is_terminal() || term == "dumb" {
        return;
    }
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let sequence = alert_sequence(
        attention,
        desktop_protocol(&term_program, &term),
        std::env::var_os("TMUX").is_some(),
    );
    let _ = out.write_all(sequence.as_bytes());
    let _ = out.flush();
}

impl App {
    /// Feed this frame's program status; returns the alert to raise, if any.
    /// Alerts fire only on a change into done / error / blocked, with the
    /// `notifications` setting on and the terminal unfocused.
    pub fn attention_alert(&mut self, status: ProgramStatus) -> Option<Attention> {
        let previous = std::mem::replace(&mut self.attention_status, status);
        let attention = Attention::for_transition(previous, status)?;
        (self.settings_screen.notifications && !self.terminal_focused).then_some(attention)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::program_status::BlockedKind;
    use claurst_core::config::Config;
    use claurst_core::cost::CostTracker;

    fn app() -> App {
        let mut app = App::new(Config::default(), CostTracker::new());
        app.settings_screen.notifications = true;
        app
    }

    #[test]
    fn alerts_only_while_unfocused_and_enabled() {
        let mut app = app();
        app.attention_alert(ProgramStatus::Working);
        assert_eq!(app.attention_alert(ProgramStatus::Done), None, "focused: no alert");

        app.terminal_focused = false;
        app.attention_alert(ProgramStatus::Working);
        assert_eq!(app.attention_alert(ProgramStatus::Done), Some(Attention::Done));

        app.settings_screen.notifications = false;
        app.attention_alert(ProgramStatus::Working);
        assert_eq!(app.attention_alert(ProgramStatus::Done), None, "setting off");
    }

    #[test]
    fn alerts_once_per_transition_into_a_waiting_state() {
        let mut app = app();
        app.terminal_focused = false;
        assert_eq!(app.attention_alert(ProgramStatus::Working), None);
        assert_eq!(
            app.attention_alert(ProgramStatus::Blocked(BlockedKind::Permission)),
            Some(Attention::Blocked(BlockedKind::Permission))
        );
        assert_eq!(
            app.attention_alert(ProgramStatus::Blocked(BlockedKind::Permission)),
            None,
            "the same dialog does not ring every frame"
        );
        assert_eq!(app.attention_alert(ProgramStatus::Working), None);
        assert_eq!(app.attention_alert(ProgramStatus::Error), Some(Attention::Failed));
        assert_eq!(app.attention_alert(ProgramStatus::Idle), None, "a cancelled turn is idle");
    }

    #[test]
    fn sequences_per_terminal() {
        assert_eq!(desktop_protocol("iTerm.app", "xterm-256color"), DesktopProtocol::Osc9);
        assert_eq!(desktop_protocol("", "xterm-kitty"), DesktopProtocol::Osc9);
        assert_eq!(desktop_protocol("", "foot"), DesktopProtocol::Osc777);
        assert_eq!(
            alert_sequence(Attention::Done, DesktopProtocol::Osc9, false),
            "\x1b]9;Claurst: Turn finished\x07\x07"
        );
        assert_eq!(
            alert_sequence(
                Attention::Blocked(BlockedKind::Question),
                DesktopProtocol::Osc777,
                false
            ),
            "\x1b]777;notify;Claurst;Question needs your answer\x07\x07"
        );
        assert_eq!(
            alert_sequence(Attention::Failed, DesktopProtocol::Osc9, true),
            "\x1bPtmux;\x1b\x1b]9;Claurst: Turn failed\x07\x1b\\\x07"
        );
    }
}
