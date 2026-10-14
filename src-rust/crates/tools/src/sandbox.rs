//! OS-level sandbox for the Bash tool (#470): macOS Seatbelt profiles.
//!
//! The SBPL files under `sandbox/` and the shape of the generated profile are
//! adapted from OpenAI Codex (`codex-rs/sandboxing/src/seatbelt.rs` and its
//! `seatbelt_*.sbpl` policies), Copyright OpenAI, licensed under the Apache
//! License 2.0 (http://www.apache.org/licenses/LICENSE-2.0).
//!
//! A profile denies everything by default, allows reading the whole disk,
//! allows writes only beneath the writable roots (minus the protected `.git`
//! and `.claurst` entries), and allows network access only when asked to.
//! Codex's fourth policy, `seatbelt_read_only_platform_defaults.sbpl`, is not
//! vendored: it only matters when reads are restricted, and here they never are.
//! The root paths are passed to `sandbox-exec` as `-D` parameters, never
//! spliced into the policy text.
//!
//! When a sandboxed command fails in a way that looks like a sandbox denial
//! ([`is_likely_sandbox_denied`], ported from codex `sandboxing/src/denial.rs`),
//! the interactive TUI asks whether to rerun it outside the sandbox once, for
//! the rest of the session, or always for its command prefix. Headless and
//! ACP runs get the failure back with a note about the sandbox instead.

use crate::{PendingPermissionRequest, ToolContext};
use claurst_core::permissions::{PermissionDecision, PermissionRequest};
use claurst_core::sandbox::{SandboxMode, SandboxSettings};
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

const SEATBELT_BASE_POLICY: &str = include_str!("sandbox/seatbelt_base_policy.sbpl");
const SEATBELT_NETWORK_POLICY: &str = include_str!("sandbox/seatbelt_network_policy.sbpl");
const SEATBELT_PREFERENCES_POLICY: &str = include_str!("sandbox/seatbelt_preferences_policy.sbpl");
/// System libcurl needs this service for TLS (codex `seatbelt.rs`).
const SEATBELT_TLS_TRUST_POLICY: &str =
    "(allow mach-lookup (global-name \"com.apple.TrustEvaluationAgent\"))\n";

/// Entries kept read-only directly beneath every writable root: the
/// repository metadata, claurst's own project settings, and the credential
/// directories that sit beneath a root when the workspace is `$HOME`.
const PROTECTED_NAMES: [&str; 5] = [".git", ".claurst", ".ssh", ".aws", ".gnupg"];

/// A generated Seatbelt profile plus the `-D` parameters it refers to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatbeltProfile {
    policy: String,
    params: Vec<(String, PathBuf)>,
}

impl SeatbeltProfile {
    /// Build the profile for `writable_roots` (empty: read-only). Writes to
    /// any path in `protected` (and anything beneath it) stay denied inside
    /// every root that contains it. Paths must already be canonical: Seatbelt
    /// matches the resolved path, so `/var/...` never matches `/private/var/...`.
    pub fn new(writable_roots: &[PathBuf], protected: &[PathBuf], network: bool) -> Self {
        let mut params = Vec::new();
        let mut sections = vec![
            SEATBELT_BASE_POLICY.to_string(),
            "; allow read-only file operations\n(allow file-read*)".to_string(),
        ];

        if !writable_roots.is_empty() {
            let mut allows = Vec::with_capacity(writable_roots.len());
            let mut anchor_denies = Vec::with_capacity(writable_roots.len());
            for (index, root) in writable_roots.iter().enumerate() {
                let key = format!("WRITABLE_ROOT_{index}");
                params.push((key.clone(), root.clone()));
                // A root must not be replaced (renamed away and recreated)
                // between commands, or the next profile would grant a new tree.
                anchor_denies.push(format!(
                    "(deny file-write-unlink (require-all (literal (param \"{key}\")) (vnode-type DIRECTORY)))"
                ));
                let mut parts = vec![format!("(subpath (param \"{key}\"))")];
                let excluded = protected
                    .iter()
                    .filter(|path| path.starts_with(root) && path.as_path() != root.as_path());
                for (excluded_index, path) in excluded.enumerate() {
                    let excluded_key = format!("{key}_EXCLUDED_{excluded_index}");
                    // `literal` too: `subpath` alone would still allow creating
                    // the protected entry itself (`mkdir .git`).
                    parts.push(format!("(require-not (literal (param \"{excluded_key}\")))"));
                    parts.push(format!("(require-not (subpath (param \"{excluded_key}\")))"));
                    params.push((excluded_key, path.clone()));
                }
                allows.push(if parts.len() == 1 {
                    parts.remove(0)
                } else {
                    format!("(require-all {} )", parts.join(" "))
                });
            }
            sections.push(format!("(allow file-write*\n{}\n)", allows.join(" ")));
            sections.extend(anchor_denies);
        }

        if network {
            sections.push(format!(
                "(allow network-outbound)\n(allow network-inbound)\n{SEATBELT_NETWORK_POLICY}{SEATBELT_TLS_TRUST_POLICY}"
            ));
        }
        sections.push(SEATBELT_PREFERENCES_POLICY.to_string());
        sections.push("(deny mach-lookup (xpc-service-name-prefix \"\"))".to_string());
        // These fcntls mutate files through read-only descriptors, bypassing
        // file-write*. F_MAKECOMPRESSED = 80, F_TRANSFEREXTENTS = 110.
        sections.push("(deny system-fcntl (fcntl-command 80 110))".to_string());

        Self {
            policy: sections.join("\n"),
            params,
        }
    }

    /// The SBPL policy text passed with `-p`.
    pub fn policy(&self) -> &str {
        &self.policy
    }

    /// `sandbox-exec` arguments up to and including the `--` separator; the
    /// command to run follows them.
    pub fn args(&self) -> Vec<OsString> {
        let mut args = Vec::with_capacity(self.params.len() + 3);
        args.push(OsString::from("-p"));
        args.push(OsString::from(&self.policy));
        for (key, path) in &self.params {
            let mut arg = OsString::from(format!("-D{key}="));
            arg.push(path);
            args.push(arg);
        }
        args.push(OsString::from("--"));
        args
    }
}

/// The protected entries for `roots`: `<root>/.git` and `<root>/.claurst`,
/// plus the resolved target when such an entry is a symlink (a `.git` file
/// or link pointing elsewhere must not become writable through the link).
pub fn protected_paths(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for root in roots {
        for name in PROTECTED_NAMES {
            let path = root.join(name);
            if let Ok(resolved) = std::fs::canonicalize(&path) {
                if resolved != path && !paths.contains(&resolved) {
                    paths.push(resolved);
                }
            }
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
    }
    paths
}

/// Canonicalize `path` for a profile, keeping it as given when it does not
/// exist (a missing root still gets a correct, if unusable, rule).
pub fn canonical_root(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// The profile for running `command` from `ctx`, or `None` when it runs
/// unsandboxed: mode `off`, no Seatbelt on this host, or the user approved
/// running it outside the sandbox for this session or its prefix.
///
/// Writable roots for `workspace-write`: the working directory, the
/// configured workspace paths and `--add-dir` directories, `TMPDIR` and
/// `/tmp`, all canonicalized.
pub(crate) fn profile_for(ctx: &ToolContext, command: &str) -> Option<SeatbeltProfile> {
    let settings = &ctx.config.sandbox;
    if approved_unsandboxed(settings, command) {
        return None;
    }
    let roots = match settings.effective_mode() {
        SandboxMode::Off => return None,
        SandboxMode::ReadOnly => Vec::new(),
        SandboxMode::WorkspaceWrite => {
            let mut roots = Vec::new();
            let temp = [
                canonical_root(&std::env::temp_dir()),
                canonical_root(Path::new("/tmp")),
            ];
            for root in ctx.workspace_roots().into_iter().chain(temp) {
                if !roots.contains(&root) {
                    roots.push(root);
                }
            }
            roots
        }
    };
    let protected = protected_paths(&roots);
    Some(SeatbeltProfile::new(&roots, &protected, settings.network))
}

// ---------------------------------------------------------------------------
// Denial detection and escalation
// ---------------------------------------------------------------------------

/// Tool name of the "retry outside the sandbox?" request pushed onto the
/// pending-permission queue. The TUI renders it as its own dialog and never
/// auto-resolves it from permission rules; ACP leaves it unanswered.
pub const SANDBOX_ESCALATION_TOOL: &str = "SandboxEscalation";

/// Exit codes that mean "usage error", "not executable" or "not found" rather
/// than a denial (codex `QUICK_REJECT_EXIT_CODES`).
const QUICK_REJECT_EXIT_CODES: [i32; 3] = [2, 126, 127];

/// Output fragments that suggest a sandbox denial (codex `SANDBOX_DENIED_KEYWORDS`).
const SANDBOX_DENIED_KEYWORDS: [&str; 7] = [
    "operation not permitted",
    "permission denied",
    "read-only file system",
    "seccomp",
    "sandbox",
    "landlock",
    "failed to write file",
];

/// What a client reports when the sandbox blocks its network access: the
/// resolver and `connect` fail rather than returning EPERM. Matched only while
/// `sandbox.network` is off, so real network outages with the network open
/// are not reported as denials.
const NETWORK_DENIED_KEYWORDS: [&str; 3] = [
    "could not resolve host",
    "couldn't connect to server",
    "network is unreachable",
];

/// Whether a failed sandboxed command was likely stopped by the sandbox.
/// There is no exact signal: a command can fail with "permission denied" for
/// its own reasons, so this is the codex heuristic: a non-zero exit that is
/// not 2, 126 or 127, with a denial keyword in the (merged PTY) output. With
/// `network` off, resolver and connect failures count as denials too.
pub fn is_likely_sandbox_denied(exit_code: i32, output: &str, network: bool) -> bool {
    if exit_code == 0 || QUICK_REJECT_EXIT_CODES.contains(&exit_code) {
        return false;
    }
    let lower = output.to_lowercase();
    SANDBOX_DENIED_KEYWORDS
        .iter()
        .chain(if network { &[][..] } else { &NETWORK_DENIED_KEYWORDS[..] })
        .any(|needle| lower.contains(needle))
}

/// Set when the user chose "outside the sandbox for this session". Process
/// wide: only the interactive TUI, one session per process, can set it.
static UNSANDBOXED_SESSION: AtomicBool = AtomicBool::new(false);
/// Prefixes approved with "always" during this run; earlier runs' choices
/// come from `sandbox.unsandboxedPrefixes` in the settings.
static UNSANDBOXED_PREFIXES: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// How far an approval to run outside the sandbox reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EscalationScope {
    /// This command, this time.
    Once,
    /// Every command for the rest of the session.
    Session,
    /// Every simple command with the same first word, persisted to the
    /// global settings like the Bash "always allow" prefixes.
    Prefix,
}

/// First words that run whatever follows them: approving one of these
/// "always" would switch the sandbox off for every command.
const LAUNCHERS: [&str; 18] = [
    "bash", "sh", "zsh", "dash", "fish", "env", "xargs", "sudo", "exec", "eval", "nohup",
    "time", "command", "python", "python3", "node", "perl", "ruby",
];

/// The prefix a "for this command prefix" approval covers: the command's
/// first word, when the command is a single simple command. Chained,
/// piped, redirected or substituted commands have none, so approving `git`
/// can never cover `git status; curl ...` or `git log > ~/.zshrc`; nor do
/// launchers such as `bash` or `env` (see [`LAUNCHERS`]).
pub fn unsandboxed_prefix(command: &str) -> Option<&str> {
    let command = command.trim();
    if command.contains(['\n', ';', '&', '|', '`', '<', '>']) || command.contains("$(") {
        return None;
    }
    command.split_whitespace().next().filter(|word| {
        let name = word.rsplit('/').next().unwrap_or(word);
        !word.contains('=') && !LAUNCHERS.contains(&name)
    })
}

/// Record the user's approval to run `command` outside the sandbox.
pub fn record_escalation(scope: EscalationScope, command: &str) {
    match scope {
        EscalationScope::Once => {}
        EscalationScope::Session => UNSANDBOXED_SESSION.store(true, Ordering::Relaxed),
        EscalationScope::Prefix => {
            let Some(prefix) = unsandboxed_prefix(command) else {
                return;
            };
            UNSANDBOXED_PREFIXES
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(prefix.to_string());
            match claurst_core::config::Settings::load_sync() {
                Ok(mut settings) => {
                    if !settings.sandbox.unsandboxed_prefixes.iter().any(|p| p == prefix) {
                        settings.sandbox.unsandboxed_prefixes.push(prefix.to_string());
                        if let Err(e) = settings.save_sync() {
                            tracing::warn!(error = %e, "could not save the unsandboxed prefix");
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "could not load settings to save the unsandboxed prefix"),
            }
        }
    }
}

/// Whether the user already approved running `command` outside the sandbox.
fn approved_unsandboxed(settings: &SandboxSettings, command: &str) -> bool {
    if UNSANDBOXED_SESSION.load(Ordering::Relaxed) {
        return true;
    }
    let Some(prefix) = unsandboxed_prefix(command) else {
        return false;
    };
    settings.unsandboxed_prefixes.iter().any(|p| p == prefix)
        || UNSANDBOXED_PREFIXES
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(prefix)
}

/// Outcome of offering to rerun a denied command outside the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryDecision {
    /// The user approved a rerun outside the sandbox.
    Approved,
    /// The user kept the denial.
    Declined,
    /// Nobody could be asked (headless, ACP, or no TUI queue).
    NotAsked,
}

/// Ask, through the interactive permission dialog, whether to rerun
/// `command` outside the sandbox. The dialog records the chosen scope itself
/// (see [`record_escalation`]); this only returns whether to rerun now.
pub(crate) async fn request_unsandboxed_retry(ctx: &ToolContext, command: &str) -> RetryDecision {
    if ctx.non_interactive {
        return RetryDecision::NotAsked;
    }
    let Some(queue) = &ctx.pending_permissions else {
        return RetryDecision::NotAsked;
    };
    let reason = "The sandbox likely blocked this command. Run it again outside the sandbox?";
    let (tx, rx) = tokio::sync::oneshot::channel();
    queue.lock().queue.push_back(PendingPermissionRequest {
        tool_use_id: format!(
            "sandbox-{}-{}",
            ctx.session_id,
            ctx.current_turn.fetch_add(1, Ordering::Relaxed)
        ),
        request: PermissionRequest {
            tool_name: SANDBOX_ESCALATION_TOOL.to_string(),
            description: reason.to_string(),
            details: None,
            is_read_only: false,
            path: Some(command.to_string()),
            working_dir: Some(ctx.working_dir.clone()),
            allowed_roots: Vec::new(),
            context_description: None,
        },
        reason: reason.to_string(),
        decision_tx: Some(tx),
    });
    match rx.await {
        Ok(PermissionDecision::Allow | PermissionDecision::AllowPermanently) => RetryDecision::Approved,
        Ok(_) => RetryDecision::Declined,
        Err(_) => RetryDecision::NotAsked,
    }
}

/// Note appended to the result of a command the sandbox likely blocked.
pub(crate) fn denial_note(settings: &SandboxSettings, decision: RetryDecision) -> String {
    let limits = match settings.mode {
        SandboxMode::ReadOnly => "it allows no writes",
        _ => {
            "writes are limited to the workspace, --add-dir directories and the temp directories, \
             with .git and .claurst read-only"
        }
    };
    let network = if settings.network { "on" } else { "off" };
    let next = match decision {
        RetryDecision::Declined => "The user chose not to rerun it outside the sandbox.",
        RetryDecision::Approved | RetryDecision::NotAsked => {
            "Rerunning it outside the sandbox needs the user's approval in the interactive TUI, a \
             `sandbox.unsandboxedPrefixes` entry or `sandbox.mode: \"off\"` in the global settings."
        }
    };
    format!(
        "\n\n[sandbox] This command ran in the macOS Seatbelt sandbox ({}) and was likely blocked by it: {}; network access is {}. {}",
        settings.mode.as_str(),
        limits,
        network,
        next
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Everything after the base policy and the file-read section.
    fn dynamic_part(profile: &SeatbeltProfile) -> &str {
        let marker = "(allow file-read*)\n";
        let start = profile.policy().find(marker).unwrap() + marker.len();
        &profile.policy()[start..]
    }

    fn arg_strings(profile: &SeatbeltProfile) -> Vec<String> {
        profile
            .args()
            .into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect()
    }

    const TAIL: &str = "(deny mach-lookup (xpc-service-name-prefix \"\"))\n\
                        (deny system-fcntl (fcntl-command 80 110))";

    #[test]
    fn profile_starts_deny_default_and_reads_everything() {
        let profile = SeatbeltProfile::new(&[], &[], false);
        let policy = profile.policy();
        assert!(policy.contains("(version 1)"));
        assert!(policy.contains("(deny default)"));
        assert!(policy.contains("Apache License 2.0"));
        assert!(policy.contains("(allow file-read*)"));
        assert!(policy.ends_with(TAIL));
    }

    #[test]
    fn snapshot_read_only_without_network() {
        let profile = SeatbeltProfile::new(&[], &[], false);
        let expected = format!("{SEATBELT_PREFERENCES_POLICY}\n{TAIL}");
        assert_eq!(dynamic_part(&profile), expected);
        assert!(!dynamic_part(&profile).contains("file-write*"));
        assert!(!dynamic_part(&profile).contains("network"));
        assert_eq!(arg_strings(&profile)[2..], ["--"]);
    }

    // `protected_paths` joins with the host separator; Seatbelt paths are POSIX.
    #[cfg(unix)]
    #[test]
    fn snapshot_workspace_write_with_protected_entries() {
        let roots = vec![PathBuf::from("/work/repo"), PathBuf::from("/private/tmp")];
        assert_eq!(
            protected_paths(&roots[..1]),
            [
                "/work/repo/.git",
                "/work/repo/.claurst",
                "/work/repo/.ssh",
                "/work/repo/.aws",
                "/work/repo/.gnupg",
            ]
            .map(PathBuf::from)
        );
        let protected = [
            "/work/repo/.git",
            "/work/repo/.claurst",
            "/private/tmp/.git",
            "/private/tmp/.claurst",
        ]
        .map(PathBuf::from);
        let profile = SeatbeltProfile::new(&roots, &protected, false);
        let expected = format!(
            "(allow file-write*\n\
             (require-all (subpath (param \"WRITABLE_ROOT_0\")) \
             (require-not (literal (param \"WRITABLE_ROOT_0_EXCLUDED_0\"))) \
             (require-not (subpath (param \"WRITABLE_ROOT_0_EXCLUDED_0\"))) \
             (require-not (literal (param \"WRITABLE_ROOT_0_EXCLUDED_1\"))) \
             (require-not (subpath (param \"WRITABLE_ROOT_0_EXCLUDED_1\"))) ) \
             (require-all (subpath (param \"WRITABLE_ROOT_1\")) \
             (require-not (literal (param \"WRITABLE_ROOT_1_EXCLUDED_0\"))) \
             (require-not (subpath (param \"WRITABLE_ROOT_1_EXCLUDED_0\"))) \
             (require-not (literal (param \"WRITABLE_ROOT_1_EXCLUDED_1\"))) \
             (require-not (subpath (param \"WRITABLE_ROOT_1_EXCLUDED_1\"))) )\n\
             )\n\
             (deny file-write-unlink (require-all (literal (param \"WRITABLE_ROOT_0\")) (vnode-type DIRECTORY)))\n\
             (deny file-write-unlink (require-all (literal (param \"WRITABLE_ROOT_1\")) (vnode-type DIRECTORY)))\n\
             {SEATBELT_PREFERENCES_POLICY}\n{TAIL}"
        );
        assert_eq!(dynamic_part(&profile), expected);
        let args = arg_strings(&profile);
        assert_eq!(args[0], "-p");
        assert_eq!(args[1], profile.policy());
        assert_eq!(
            args[2..],
            [
                "-DWRITABLE_ROOT_0=/work/repo",
                "-DWRITABLE_ROOT_0_EXCLUDED_0=/work/repo/.git",
                "-DWRITABLE_ROOT_0_EXCLUDED_1=/work/repo/.claurst",
                "-DWRITABLE_ROOT_1=/private/tmp",
                "-DWRITABLE_ROOT_1_EXCLUDED_0=/private/tmp/.git",
                "-DWRITABLE_ROOT_1_EXCLUDED_1=/private/tmp/.claurst",
                "--",
            ]
        );
    }

    /// A workspace inside another writable root (a repo under TMPDIR): the
    /// outer root's rule must also exclude the inner root's `.git`, or the
    /// union of the two rules would leave it writable.
    #[test]
    fn snapshot_nested_root_inherits_inner_protected_entries() {
        let roots = vec![PathBuf::from("/tmp/T/repo"), PathBuf::from("/tmp/T")];
        let protected = vec![PathBuf::from("/tmp/T/repo/.git")];
        let profile = SeatbeltProfile::new(&roots, &protected, false);
        assert!(dynamic_part(&profile).starts_with(
            "(allow file-write*\n\
             (require-all (subpath (param \"WRITABLE_ROOT_0\")) \
             (require-not (literal (param \"WRITABLE_ROOT_0_EXCLUDED_0\"))) \
             (require-not (subpath (param \"WRITABLE_ROOT_0_EXCLUDED_0\"))) ) \
             (require-all (subpath (param \"WRITABLE_ROOT_1\")) \
             (require-not (literal (param \"WRITABLE_ROOT_1_EXCLUDED_0\"))) \
             (require-not (subpath (param \"WRITABLE_ROOT_1_EXCLUDED_0\"))) )\n)"
        ));
        assert_eq!(
            arg_strings(&profile)[2..],
            [
                "-DWRITABLE_ROOT_0=/tmp/T/repo",
                "-DWRITABLE_ROOT_0_EXCLUDED_0=/tmp/T/repo/.git",
                "-DWRITABLE_ROOT_1=/tmp/T",
                "-DWRITABLE_ROOT_1_EXCLUDED_0=/tmp/T/repo/.git",
                "--",
            ]
        );
    }

    #[test]
    fn snapshot_root_without_protected_entries_is_a_plain_subpath() {
        let profile = SeatbeltProfile::new(&[PathBuf::from("/work")], &[], false);
        assert!(dynamic_part(&profile).starts_with(
            "(allow file-write*\n(subpath (param \"WRITABLE_ROOT_0\"))\n)\n\
             (deny file-write-unlink (require-all (literal (param \"WRITABLE_ROOT_0\")) (vnode-type DIRECTORY)))\n"
        ));
    }

    #[test]
    fn snapshot_network_section_only_when_enabled() {
        let roots = [PathBuf::from("/work")];
        let off = SeatbeltProfile::new(&roots, &[], false);
        let on = SeatbeltProfile::new(&roots, &[], true);
        assert!(!dynamic_part(&off).contains("network"));
        let network = format!(
            "(allow network-outbound)\n(allow network-inbound)\n{SEATBELT_NETWORK_POLICY}{SEATBELT_TLS_TRUST_POLICY}\n{SEATBELT_PREFERENCES_POLICY}\n{TAIL}"
        );
        assert!(dynamic_part(&on).ends_with(&network));
        assert_eq!(off.args()[2..], on.args()[2..]);
    }

    #[test]
    fn denial_needs_a_failure_and_a_keyword() {
        for network in [false, true] {
            assert!(is_likely_sandbox_denied(1, "bash: /x: Operation not permitted", network));
            assert!(is_likely_sandbox_denied(1, "touch: /x: Permission denied", network));
            assert!(is_likely_sandbox_denied(128, "fatal: Unable to create '.git/index.lock': Operation not permitted", network));
            assert!(is_likely_sandbox_denied(1, "error: Read-only file system", network));
            assert!(is_likely_sandbox_denied(71, "sandbox-exec: sandbox_apply: Operation not permitted", network));
            assert!(!is_likely_sandbox_denied(0, "Operation not permitted", network));
            assert!(!is_likely_sandbox_denied(1, "error: tests failed", network));
            for code in QUICK_REJECT_EXIT_CODES {
                assert!(!is_likely_sandbox_denied(code, "Permission denied", network));
            }
        }
    }

    /// With the network off, resolver and connect failures are denials;
    /// with it on they are ordinary network errors.
    #[test]
    fn network_failures_count_only_while_network_is_off() {
        for (code, output) in [
            (6, "curl: (6) Could not resolve host: example.com"),
            (7, "curl: (7) Couldn't connect to server"),
            (128, "fatal: unable to access 'https://github.com/x/y/': Could not resolve host: github.com"),
            (1, "connect: Network is unreachable"),
        ] {
            assert!(is_likely_sandbox_denied(code, output, false), "{output}");
            assert!(!is_likely_sandbox_denied(code, output, true), "{output}");
        }
    }

    #[test]
    fn prefix_only_for_simple_commands() {
        assert_eq!(unsandboxed_prefix("git commit -m x"), Some("git"));
        assert_eq!(unsandboxed_prefix("  cargo build"), Some("cargo"));
        for chained in [
            "git status; curl evil",
            "git log && rm -rf x",
            "git log | sh",
            "git log > ~/.zshrc",
            "echo $(id)",
            "echo `id`",
            "git status\nrm x",
            "make &",
            "FOO=1 make",
            "bash -c 'rm -rf ~'",
            "/bin/sh x.sh",
            "env rm x",
            "xargs rm",
            "python3 evil.py",
            "",
        ] {
            assert_eq!(unsandboxed_prefix(chained), None, "{chained}");
        }
    }

    #[test]
    fn settings_prefixes_approve_matching_simple_commands() {
        let settings = SandboxSettings {
            unsandboxed_prefixes: vec!["npm".to_string()],
            ..SandboxSettings::default()
        };
        assert!(approved_unsandboxed(&settings, "npm install"));
        assert!(!approved_unsandboxed(&settings, "npm install; curl x"));
        assert!(!approved_unsandboxed(&settings, "npx foo"));
        assert!(!approved_unsandboxed(&SandboxSettings::default(), "npm install"));
    }

    #[test]
    fn denial_note_names_mode_network_and_next_step() {
        let settings = SandboxSettings::default();
        let note = denial_note(&settings, RetryDecision::NotAsked);
        assert!(note.starts_with("\n\n[sandbox] "));
        assert!(note.contains("(workspace-write)"));
        assert!(note.contains("network access is off"));
        assert!(note.contains("sandbox.unsandboxedPrefixes"));
        let declined = denial_note(
            &SandboxSettings {
                mode: SandboxMode::ReadOnly,
                network: true,
                ..SandboxSettings::default()
            },
            RetryDecision::Declined,
        );
        assert!(declined.contains("it allows no writes"));
        assert!(declined.contains("network access is on"));
        assert!(declined.contains("chose not to rerun"));
    }

    /// Without a TUI queue (headless) nobody is asked.
    #[tokio::test]
    async fn headless_never_asks() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = crate::test_support::allow_all_context(dir.path().to_path_buf());
        assert_eq!(request_unsandboxed_retry(&ctx, "touch /x").await, RetryDecision::NotAsked);
        ctx.non_interactive = false;
        assert_eq!(request_unsandboxed_retry(&ctx, "touch /x").await, RetryDecision::NotAsked);
    }

    /// Interactive: the request lands on the pending queue as a
    /// `SandboxEscalation` and the answer decides the rerun.
    #[tokio::test]
    async fn interactive_request_goes_through_the_permission_queue() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = crate::test_support::allow_all_context(dir.path().to_path_buf());
        ctx.non_interactive = false;
        let store = std::sync::Arc::new(parking_lot::Mutex::new(
            crate::PendingPermissionStore::default(),
        ));
        ctx.pending_permissions = Some(store.clone());
        for (answer, expected) in [
            (PermissionDecision::Allow, RetryDecision::Approved),
            (PermissionDecision::Deny, RetryDecision::Declined),
        ] {
            let answerer = {
                let store = store.clone();
                tokio::spawn(async move {
                    loop {
                        let pending = store.lock().queue.pop_front();
                        if let Some(mut pending) = pending {
                            assert_eq!(pending.request.tool_name, SANDBOX_ESCALATION_TOOL);
                            assert_eq!(pending.request.path.as_deref(), Some("touch /x"));
                            let _ = pending.decision_tx.take().unwrap().send(answer);
                            return;
                        }
                        tokio::task::yield_now().await;
                    }
                })
            };
            assert_eq!(request_unsandboxed_retry(&ctx, "touch /x").await, expected);
            answerer.await.unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn protected_paths_add_the_target_of_a_symlinked_entry() {
        let dir = tempfile::tempdir().unwrap();
        let root = canonical_root(dir.path());
        let elsewhere = root.join("gitdir");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, root.join(".git")).unwrap();
        let protected = protected_paths(std::slice::from_ref(&root));
        assert_eq!(
            protected,
            [
                elsewhere,
                root.join(".git"),
                root.join(".claurst"),
                root.join(".ssh"),
                root.join(".aws"),
                root.join(".gnupg"),
            ]
        );
    }
}
