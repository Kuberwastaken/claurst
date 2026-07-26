// Session-navigation commands ported from opencode: `/new` and `/move`.
//
// `/new`  mirrors opencode's `session.new` (packages/tui/src/app.tsx): it clears
//         the view to a blank "home" and begins a brand-new session, preserving
//         the current model / provider / effort selection and the working
//         directory. It is *lazy* — the fresh session is not written to disk
//         until the first message arrives (ConversationSession is only persisted
//         after a turn completes), matching opencode's lazy-home semantics.
//
// `/move` mirrors opencode's `ControlPlaneMoveSession.moveSession`
//         (packages/core/src/control-plane/move-session.ts): it re-homes the
//         current session to a DIFFERENT worktree/directory of the SAME project,
//         carrying over uncommitted changes and resetting them in the old
//         location.
//
// Architectural adaptation vs opencode:
//   * opencode models a first-class Project (one git repo) with many worktrees
//     tracked in a control plane, and a session carries a `location.directory`.
//     claurst has no project/worktree registry — a session simply carries a
//     `working_dir`. We therefore identify "the same project" by the git *common
//     directory* (`git rev-parse --git-common-dir`), which is shared by every
//     linked worktree of a repo, and take the destination as a `/move <dir>`
//     argument instead of an interactive worktree picker.
//   * opencode injects a synthetic `<system-reminder>` prompt after a move so the
//     model learns the cwd changed. claurst re-derives the working directory into
//     every turn's system prompt (crates/query working_directory +
//     crates/cli qcfg.working_directory), so re-homing the working_dir already
//     informs the model on its next turn; we surface the move as a status line
//     instead of appending a dangling user message that would break Anthropic's
//     user/assistant role alternation.

use super::{CommandContext, CommandResult, SlashCommand};
use async_trait::async_trait;
use claurst_core::git_utils::get_repo_root;
use claurst_core::history::ConversationSession;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// /new — lazy-home / fresh session
// ---------------------------------------------------------------------------

pub struct NewCommand;

#[async_trait]
impl SlashCommand for NewCommand {
    fn name(&self) -> &str {
        "new"
    }

    fn description(&self) -> &str {
        "Start a fresh session — keeps your model, provider and directory"
    }

    fn help(&self) -> &str {
        "Usage: /new\n\n\
         Clears the transcript to a blank home and begins a brand-new session. \
         Your model, provider, effort level and working directory carry over, and \
         the new session is not written to disk until your first message (a \
         \"lazy\" session).\n\n\
         Compare with /clear, which wipes the transcript but keeps the same \
         session id (continuing the same on-disk history)."
    }

    async fn execute(&self, _args: &str, _ctx: &mut CommandContext) -> CommandResult {
        CommandResult::NewSession
    }
}

/// Build the fresh "home" session for `/new`: a brand-new session id with an
/// empty transcript, preserving the current model and working directory.
///
/// Mirrors opencode's lazy `session.new` — the returned session is not persisted
/// here; the REPL only writes it once the first message completes a turn.
pub fn build_home_session(model: String, working_dir: Option<String>) -> ConversationSession {
    let mut session = ConversationSession::new(model);
    session.working_dir = working_dir;
    session
}

// ---------------------------------------------------------------------------
// /move — re-home the session to another worktree of the same project
// ---------------------------------------------------------------------------

pub struct MoveCommand;

#[async_trait]
impl SlashCommand for MoveCommand {
    fn name(&self) -> &str {
        "move"
    }

    fn description(&self) -> &str {
        "Re-home this session to another worktree of the same project"
    }

    fn help(&self) -> &str {
        "Usage: /move [--no-changes] <directory>\n\n\
         Moves the current session to another directory (typically a git \
         worktree) of the SAME project, carrying your uncommitted changes across \
         and resetting them in the old location. The model is told about the new \
         working directory on its next turn.\n\n\
         Pass --no-changes to re-home without moving working-tree changes.\n\n\
         Examples:\n\
         \x20 /move ../myapp-feature\n\
         \x20 /move --no-changes /path/to/other/worktree"
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        // ---- argument parsing (dir + optional --no-changes flag) -----------
        let mut move_changes = true;
        let mut positional: Vec<&str> = Vec::new();
        for token in args.split_whitespace() {
            match token {
                "--no-changes" | "--keep-changes" | "--keep" => move_changes = false,
                other => positional.push(other),
            }
        }
        if positional.is_empty() {
            return CommandResult::Message(
                "Usage: /move [--no-changes] <directory>\n\
                 Re-homes this session to another worktree of the same project."
                    .to_string(),
            );
        }
        let target_raw = positional.join(" ");

        // ---- resolve destination -------------------------------------------
        let mut dest = expand_tilde(&target_raw);
        if dest.is_relative() {
            dest = ctx.working_dir.join(dest);
        }
        if !dest.exists() {
            return CommandResult::Error(format!("Directory does not exist: {}", dest.display()));
        }
        if !dest.is_dir() {
            return CommandResult::Error(format!("Not a directory: {}", dest.display()));
        }
        let dest = dest.canonicalize().unwrap_or(dest);
        let source = ctx
            .working_dir
            .canonicalize()
            .unwrap_or_else(|_| ctx.working_dir.clone());

        // Same directory → no-op (matches opencode's early return).
        if dest == source {
            return CommandResult::Message(format!(
                "Session is already located at {}",
                dest.display()
            ));
        }

        // ---- same-project check (opencode's DestinationProjectMismatch) ----
        // Two worktrees of one repo share a git common directory; require it to
        // match so /move only re-homes within the same project. When the source
        // isn't a git repo there is no project identity to enforce.
        let source_common = git_common_dir(&source);
        if let Some(source_common) = source_common.as_ref() {
            match git_common_dir(&dest) {
                Some(dest_common) if &dest_common != source_common => {
                    return CommandResult::Error(
                        "Destination belongs to a different project. /move only \
                         re-homes a session between worktrees of the same repository."
                            .to_string(),
                    );
                }
                None => {
                    return CommandResult::Error(
                        "Destination is not a git repository, so it can't be a \
                         worktree of this project."
                            .to_string(),
                    );
                }
                _ => {}
            }
        }

        // ---- carry uncommitted changes -------------------------------------
        match move_session_changes(&source, &dest, move_changes) {
            Ok(moved_changes) => CommandResult::MoveSession {
                destination: dest,
                moved_changes,
            },
            Err(err) => CommandResult::Error(format!("Move failed: {err}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Git change relocation (faithful port of git.change.capture/apply/discard)
// ---------------------------------------------------------------------------

/// Errors raised while relocating uncommitted changes between worktrees.
#[derive(Debug)]
pub enum MoveError {
    /// The source directory is not inside a git repository.
    SourceNotAGitRepo(PathBuf),
    /// The destination directory is not inside a git repository.
    DestNotAGitRepo(PathBuf),
    /// `git diff`/`ls-files` failed while capturing the source changes.
    Capture(String),
    /// `git apply` failed while applying the patch to the destination.
    Apply(String),
    /// `git checkout`/`git clean` failed while resetting the source.
    Reset(String),
}

impl fmt::Display for MoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MoveError::SourceNotAGitRepo(p) => {
                write!(f, "source is not a git repository ({})", p.display())
            }
            MoveError::DestNotAGitRepo(p) => {
                write!(f, "destination is not a git repository ({})", p.display())
            }
            MoveError::Capture(m) => write!(f, "could not capture changes: {m}"),
            MoveError::Apply(m) => write!(f, "could not apply changes: {m}"),
            MoveError::Reset(m) => write!(f, "could not reset the source: {m}"),
        }
    }
}

impl std::error::Error for MoveError {}

/// Relocate uncommitted changes from `source` to `dest` and reset the source,
/// mirroring opencode's `MoveSession.moveSession` change handling.
///
/// Captures tracked (`git diff --binary HEAD`) and untracked
/// (`git ls-files --others` + `git diff --no-index`) changes scoped to the
/// session directory, applies them to the destination worktree, then discards
/// them from the source (`git checkout --` preserving the index, `git clean -fd`
/// removing untracked files).
///
/// Returns `true` when any changes were carried across, `false` when there was
/// nothing to move (or `move_changes` was `false`).
pub fn move_session_changes(source: &Path, dest: &Path, move_changes: bool) -> Result<bool, MoveError> {
    if !move_changes {
        return Ok(false);
    }
    let source_root =
        get_repo_root(source).ok_or_else(|| MoveError::SourceNotAGitRepo(source.to_path_buf()))?;
    let dest_root =
        get_repo_root(dest).ok_or_else(|| MoveError::DestNotAGitRepo(dest.to_path_buf()))?;

    let scope = scope_of(&source_root, source);
    let patch = capture_changes(&source_root, &scope)?;
    if patch.trim().is_empty() {
        return Ok(false);
    }
    apply_changes(&dest_root, &patch)?;
    reset_source(&source_root, &scope)?;
    Ok(true)
}

/// Path of `dir` relative to the repository `root`, forward-slashed, or `"."`
/// when `dir` is the root itself. Matches the `scope` git uses in opencode.
fn scope_of(root: &Path, dir: &Path) -> String {
    match dir.strip_prefix(root) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => ".".to_string(),
    }
}

fn run_git(dir: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("git").current_dir(dir).args(args).output()
}

/// Capture tracked + untracked changes under `scope` as a single git patch.
fn capture_changes(source_root: &Path, scope: &str) -> Result<String, MoveError> {
    // Tracked changes (staged + unstaged) versus HEAD.
    let tracked = run_git(source_root, &["diff", "--binary", "HEAD", "--", scope])
        .map_err(|e| MoveError::Capture(e.to_string()))?;
    if !tracked.status.success() {
        return Err(MoveError::Capture(git_stderr(
            &tracked,
            "failed to capture tracked changes",
        )));
    }
    let mut parts = vec![String::from_utf8_lossy(&tracked.stdout).into_owned()];

    // Untracked files, NUL-separated so paths with newlines survive.
    let untracked = run_git(
        source_root,
        &["ls-files", "--others", "--exclude-standard", "-z", "--", scope],
    )
    .map_err(|e| MoveError::Capture(e.to_string()))?;
    if !untracked.status.success() {
        return Err(MoveError::Capture(git_stderr(
            &untracked,
            "failed to list untracked changes",
        )));
    }

    for file in String::from_utf8_lossy(&untracked.stdout)
        .split('\0')
        .filter(|s| !s.is_empty())
    {
        let created = run_git(source_root, &["diff", "--binary", "--no-index", "--", "/dev/null", file])
            .map_err(|e| MoveError::Capture(e.to_string()))?;
        // `git diff --no-index` exits 1 when it finds differences (the normal
        // case for a brand-new file); 0 or 1 are both success here.
        match created.status.code() {
            Some(0) | Some(1) => parts.push(String::from_utf8_lossy(&created.stdout).into_owned()),
            _ => {
                return Err(MoveError::Capture(git_stderr(
                    &created,
                    &format!("failed to capture untracked change: {file}"),
                )))
            }
        }
    }

    Ok(parts
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Apply a captured patch to the destination worktree via `git apply -`.
fn apply_changes(dest_root: &Path, patch: &str) -> Result<(), MoveError> {
    use std::io::Write;
    use std::process::Stdio;

    let mut child = Command::new("git")
        .current_dir(dest_root)
        .args(["apply", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| MoveError::Apply(e.to_string()))?;

    // Stream the patch on a separate thread so a large patch can't deadlock
    // against git filling its stdout/stderr pipes.
    let mut stdin = child.stdin.take().expect("stdin was requested as piped");
    let patch_owned = patch.to_string();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(patch_owned.as_bytes());
        // `stdin` drops here, closing the pipe so git sees EOF.
    });

    let output = child
        .wait_with_output()
        .map_err(|e| MoveError::Apply(e.to_string()))?;
    let _ = writer.join();

    if output.status.success() {
        Ok(())
    } else {
        Err(MoveError::Apply(git_stderr(&output, "git apply failed")))
    }
}

/// Discard the moved changes from the source: restore tracked files from the
/// index (preserving staged state) and remove untracked files.
fn reset_source(source_root: &Path, scope: &str) -> Result<(), MoveError> {
    let checkout = run_git(source_root, &["checkout", "--", scope])
        .map_err(|e| MoveError::Reset(e.to_string()))?;
    if !checkout.status.success() {
        return Err(MoveError::Reset(git_stderr(
            &checkout,
            "failed to restore tracked changes",
        )));
    }
    let clean = run_git(source_root, &["clean", "-fd", "--", scope])
        .map_err(|e| MoveError::Reset(e.to_string()))?;
    if !clean.status.success() {
        return Err(MoveError::Reset(git_stderr(
            &clean,
            "failed to remove untracked changes",
        )));
    }
    Ok(())
}

fn git_stderr(output: &std::process::Output, fallback: &str) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Expand a leading `~` to the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    } else if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(path)
}

/// Absolute git *common directory* for `start`, shared by every linked worktree
/// of a repository. Used to decide whether two directories belong to the same
/// project (opencode compares `projectID`).
pub fn git_common_dir(start: &Path) -> Option<PathBuf> {
    let root = get_repo_root(start)?;

    // Prefer an absolute answer (git >= 2.31).
    let abs = Command::new("git")
        .current_dir(&root)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(s) = abs {
        let p = PathBuf::from(&s);
        return Some(p.canonicalize().unwrap_or(p));
    }

    // Fallback for older git: resolve the (possibly relative) common dir.
    let rel = Command::new("git")
        .current_dir(&root)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())?;
    let p = PathBuf::from(&rel);
    let abs = if p.is_absolute() { p } else { root.join(p) };
    Some(abs.canonicalize().unwrap_or(abs))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // ---- /new home-state reset ------------------------------------------

    #[test]
    fn build_home_session_preserves_model_and_dir() {
        let old = {
            let mut s = ConversationSession::new("claude-sonnet-4-6".to_string());
            s.working_dir = Some("/tmp/project".to_string());
            s.messages.push(claurst_core::types::Message::user("hi"));
            s.title = Some("Old title".to_string());
            s
        };

        let fresh = build_home_session(old.model.clone(), old.working_dir.clone());

        // Model + working directory carry over (provider/effort live in Config).
        assert_eq!(fresh.model, "claude-sonnet-4-6");
        assert_eq!(fresh.working_dir.as_deref(), Some("/tmp/project"));
        // Genuinely new session: different id, empty transcript, no title.
        assert_ne!(fresh.id, old.id);
        assert!(fresh.messages.is_empty());
        assert!(fresh.title.is_none());
    }

    #[test]
    fn build_home_session_generates_unique_ids() {
        let a = build_home_session("m".to_string(), None);
        let b = build_home_session("m".to_string(), None);
        assert_ne!(a.id, b.id);
    }

    // ---- /move change relocation ----------------------------------------

    fn git_ok(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Init a repo with one commit; returns the canonical repo root, or `None`
    /// when git is unavailable (so the test degenerates to a no-op).
    fn init_repo(dir: &Path) -> Option<PathBuf> {
        if !git_ok(dir, &["init", "-q"]) {
            return None;
        }
        git_ok(dir, &["config", "user.email", "test@example.com"]);
        git_ok(dir, &["config", "user.name", "Test"]);
        // Pin line-ending handling so the round-trip is byte-exact regardless of
        // the host's global git config. GitHub's Windows runners set
        // core.autocrlf=true globally, which would rewrite the moved file's LF to
        // CRLF on checkout into the destination worktree and break the content
        // assertions below. This config lives in the shared common dir, so it
        // also governs any worktree added off this repo.
        git_ok(dir, &["config", "core.autocrlf", "false"]);
        git_ok(dir, &["config", "core.eol", "lf"]);
        fs::write(dir.join("tracked.txt"), "original\n").ok()?;
        if !git_ok(dir, &["add", "-A"]) {
            return None;
        }
        if !git_ok(dir, &["commit", "-q", "-m", "init"]) {
            return None;
        }
        dir.canonicalize().ok()
    }

    #[test]
    fn move_changes_disabled_is_noop() {
        assert!(!move_session_changes(
            Path::new("/nonexistent-a"),
            Path::new("/nonexistent-b"),
            false
        )
        .unwrap());
    }

    #[test]
    fn move_carries_and_resets_uncommitted_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let Some(repo) = init_repo(&repo) else {
            eprintln!("git unavailable; skipping move_carries_and_resets_uncommitted_changes");
            return;
        };

        // Add a second worktree of the SAME project.
        let wt2 = tmp.path().join("wt2");
        if !git_ok(&repo, &["worktree", "add", "-q", "--detach", wt2.to_str().unwrap()]) {
            eprintln!("git worktree unsupported; skipping test");
            return;
        }
        let wt2 = wt2.canonicalize().unwrap();

        // Same project → identical git common directory.
        assert_eq!(git_common_dir(&repo), git_common_dir(&wt2));

        // Uncommitted work in the source: modify a tracked file + add an
        // untracked one.
        fs::write(repo.join("tracked.txt"), "modified\n").unwrap();
        fs::write(repo.join("untracked.txt"), "brand new\n").unwrap();

        let moved = move_session_changes(&repo, &wt2, true).unwrap();
        assert!(moved, "expected changes to be carried across");

        // Destination now has both changes.
        assert_eq!(fs::read_to_string(wt2.join("tracked.txt")).unwrap(), "modified\n");
        assert_eq!(fs::read_to_string(wt2.join("untracked.txt")).unwrap(), "brand new\n");

        // Source is reset: tracked file restored, untracked removed.
        assert_eq!(fs::read_to_string(repo.join("tracked.txt")).unwrap(), "original\n");
        assert!(!repo.join("untracked.txt").exists());
    }

    #[test]
    fn move_with_no_changes_returns_false() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&repo).unwrap();
        let Some(repo) = init_repo(&repo) else {
            eprintln!("git unavailable; skipping move_with_no_changes_returns_false");
            return;
        };
        let wt2 = tmp.path().join("wt2");
        if !git_ok(&repo, &["worktree", "add", "-q", "--detach", wt2.to_str().unwrap()]) {
            return;
        }
        let wt2 = wt2.canonicalize().unwrap();
        // Clean working tree → nothing to move.
        assert!(!move_session_changes(&repo, &wt2, true).unwrap());
    }
}
