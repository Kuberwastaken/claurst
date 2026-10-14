//! OS-level sandbox settings for shell commands (#470).
//!
//! The Bash tool runs each command under macOS Seatbelt (`sandbox-exec`)
//! unless the mode is `off`; the profile itself is built in `claurst-tools`.
//! Elsewhere there is no OS sandbox yet and the effective mode is `off`.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::OnceLock;

/// Only `/usr/bin/sandbox-exec` is used, never one found on `PATH`, so a
/// planted binary earlier on `PATH` cannot replace the sandbox (codex pins
/// the same path).
pub const SEATBELT_EXECUTABLE: &str = "/usr/bin/sandbox-exec";

/// What a sandboxed shell command may write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    /// No sandbox: commands run with the user's full privileges.
    Off,
    /// Writes only beneath the workspace roots, `--add-dir` directories and
    /// the temp directories; `.git` and `.claurst` stay read-only.
    #[default]
    WorkspaceWrite,
    /// No writes at all.
    ReadOnly,
}

impl SandboxMode {
    /// The settings-file spelling (`off`, `workspace-write`, `read-only`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::WorkspaceWrite => "workspace-write",
            Self::ReadOnly => "read-only",
        }
    }
}

/// The top-level `sandbox` block of the global settings file.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SandboxSettings {
    #[serde(default)]
    pub mode: SandboxMode,
    /// Allow network access from sandboxed commands. Off by default.
    #[serde(default)]
    pub network: bool,
    /// First words of commands the user chose to always run outside the
    /// sandbox from the denial prompt.
    #[serde(
        default,
        rename = "unsandboxedPrefixes",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub unsandboxed_prefixes: Vec<String>,
}

impl SandboxSettings {
    /// The mode actually applied: `mode` when Seatbelt is available, else `off`.
    pub fn effective_mode(&self) -> SandboxMode {
        if seatbelt_available() {
            self.mode
        } else {
            SandboxMode::Off
        }
    }
}

/// One line for the dynamic system prompt describing the sandbox the
/// model's Bash commands run in, or `None` when they run unsandboxed.
pub fn render_sandbox_state(settings: &SandboxSettings) -> Option<String> {
    let writes = match settings.effective_mode() {
        SandboxMode::Off => return None,
        SandboxMode::WorkspaceWrite => {
            "can write only inside the workspace, --add-dir directories and the temp \
             directories (.git and .claurst stay read-only)"
        }
        SandboxMode::ReadOnly => "cannot write files",
    };
    let (network, blocked) = if settings.network {
        ("on", "\"Operation not permitted\"")
    } else {
        (
            "off",
            "\"Operation not permitted\", or for network access with resolver or connection errors \
             (\"Could not resolve host\")",
        )
    };
    Some(format!(
        "# Sandbox\nBash commands run in a macOS Seatbelt sandbox ({}): they can read any file but {writes}; network access is {network}. A command the sandbox blocks fails with {blocked}; say so instead of working around it, so the user can approve a rerun outside the sandbox.",
        settings.mode.as_str()
    ))
}

/// Short footer badge for the active sandbox, or `None` when commands run
/// unsandboxed.
pub fn status_label(settings: &SandboxSettings) -> Option<String> {
    let mode = settings.effective_mode();
    if mode == SandboxMode::Off {
        return None;
    }
    let network = if settings.network { " +net" } else { "" };
    Some(format!("sandbox: {}{network}", mode.as_str()))
}

/// Whether this host can run commands under Seatbelt.
pub fn seatbelt_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE
        .get_or_init(|| cfg!(target_os = "macos") && Path::new(SEATBELT_EXECUTABLE).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parse_kebab_case_modes_and_default_to_workspace_write() {
        let parsed: SandboxSettings =
            serde_json::from_str(r#"{"mode": "read-only", "network": true}"#).unwrap();
        assert_eq!(parsed.mode, SandboxMode::ReadOnly);
        assert!(parsed.network);
        let empty: SandboxSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, SandboxSettings::default());
        assert_eq!(empty.mode, SandboxMode::WorkspaceWrite);
        assert!(!empty.network);
        for mode in [SandboxMode::Off, SandboxMode::WorkspaceWrite, SandboxMode::ReadOnly] {
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(json, format!("\"{}\"", mode.as_str()));
        }
    }

    #[test]
    fn prompt_line_and_badge_follow_the_effective_mode() {
        let off = SandboxSettings {
            mode: SandboxMode::Off,
            ..SandboxSettings::default()
        };
        assert_eq!(render_sandbox_state(&off), None);
        assert_eq!(status_label(&off), None);

        let write = SandboxSettings::default();
        let read_only = SandboxSettings {
            mode: SandboxMode::ReadOnly,
            network: true,
            ..SandboxSettings::default()
        };
        if !seatbelt_available() {
            assert_eq!(render_sandbox_state(&write), None);
            assert_eq!(status_label(&read_only), None);
            return;
        }
        let line = render_sandbox_state(&write).unwrap();
        assert!(line.starts_with("# Sandbox\n"));
        assert_eq!(line.lines().count(), 2);
        assert!(line.contains("(workspace-write)"));
        assert!(line.contains("network access is off"));
        assert!(line.contains("Could not resolve host"));
        assert_eq!(status_label(&write).as_deref(), Some("sandbox: workspace-write"));

        let line = render_sandbox_state(&read_only).unwrap();
        assert!(line.contains("cannot write files"));
        assert!(line.contains("network access is on"));
        assert_eq!(status_label(&read_only).as_deref(), Some("sandbox: read-only +net"));
    }

    #[test]
    fn effective_mode_is_off_without_seatbelt() {
        let settings = SandboxSettings {
            mode: SandboxMode::ReadOnly,
            ..SandboxSettings::default()
        };
        let expected = if seatbelt_available() {
            SandboxMode::ReadOnly
        } else {
            SandboxMode::Off
        };
        assert_eq!(settings.effective_mode(), expected);
        assert_eq!(seatbelt_available(), cfg!(target_os = "macos") && Path::new(SEATBELT_EXECUTABLE).is_file());
    }
}
