// PTY-backed Bash tool: wraps every command in a real pseudo-terminal so that
// programs that query isatty() (npm, cargo, git, pytest, …) behave correctly.
//
// Shell state (cwd + env) is persisted across calls through the same sentinel
// mechanism as the original BashTool, so `cd` and `export` work as expected.
//
// Platform notes
// ──────────────
//  Unix  → portable_pty (native openpty)
//  Windows → falls back to the existing cmd.exe approach; ConPTY is available
//             in portable_pty but adds complexity for minimal gain on Windows.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult, session_shell_state};
use async_trait::async_trait;
use claurst_core::bash_classifier::{BashRiskLevel, classify_bash_command};
use claurst_core::tasks::{BackgroundTask, global_registry};
use claurst_core::truncate::bound_head_tail;
use crate::output_spill::{
    Captured, OutputAccumulator, RAW_HEAD_BYTES, RAW_TAIL_BYTES, finish_spill, spill_bytes,
    with_full_output,
};
use std::path::Path;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::debug;

// Unix-only imports used by the shell-state helpers and PTY execution path.
#[cfg(unix)]
use crate::ShellState;
#[cfg(unix)]
use claurst_core::truncate::{head_window, join_head_tail, tail_window, OUTPUT_WINDOW_BYTES, OUTPUT_WINDOW_LINES};
#[cfg(unix)]
use regex::Regex;
#[cfg(unix)]
use std::collections::HashMap;

/// Sentinel appended to the shell wrapper script (Unix only).
#[cfg(unix)]
const SHELL_STATE_SENTINEL: &str = "__CC_SHELL_STATE__";

pub struct PtyBashTool;

/// Environment every shell child gets (#442): pagers are disabled so
/// `git log`, `git diff` or `man` cannot hang in `less`, git never prompts for
/// credentials on the terminal, and scripts can tell they run under an agent.
/// On the PTY path these are applied before the restored session env, so an
/// explicit `export` from an earlier call still wins.
const CHILD_ENV: [(&str, &str); 6] = [
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("GH_PAGER", "cat"),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("CLAURST", "1"),
    ("AGENT", "1"),
];

/// `TERM` for the child when the parent environment has none.
const DEFAULT_TERM: &str = "dumb";

#[derive(Debug, Deserialize)]
struct BashInput {
    command: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default = "default_timeout")]
    timeout: u64,
    #[serde(default)]
    run_in_background: bool,
}

fn default_timeout() -> u64 {
    120_000
}

// ---------------------------------------------------------------------------
// Shell state helpers — Unix only (used by the PTY wrapper script)
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn parse_shell_state_block(lines: &[String]) -> Option<(PathBuf, HashMap<String, String>)> {
    let mut iter = lines.iter();
    let cwd_line = iter.next()?;
    let cwd = PathBuf::from(cwd_line.trim());

    let mut env_vars = HashMap::new();
    for line in iter {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(eq) = line.find('=') {
            let key = line[..eq].to_string();
            let val = line[eq + 1..].to_string();
            if !key.starts_with('_')
                && !["SHLVL", "BASH_LINENO", "BASH_SOURCE", "FUNCNAME", "PIPESTATUS", "OLDPWD"]
                    .contains(&key.as_str())
            {
                env_vars.insert(key, val);
            }
        }
    }

    Some((cwd, env_vars))
}

#[cfg(unix)]
fn extract_exports_from_command(command: &str) -> HashMap<String, String> {
    let re = Regex::new(
        r#"(?m)^\s*export\s+([A-Za-z_][A-Za-z0-9_]*)=(?:"([^"]*)"|'([^']*)'|(\S*))"#,
    )
    .unwrap();
    let mut map = HashMap::new();
    for cap in re.captures_iter(command) {
        let key = cap[1].to_string();
        let val = cap
            .get(2)
            .or_else(|| cap.get(3))
            .or_else(|| cap.get(4))
            .map(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        map.insert(key, val);
    }
    map
}

// SECURITY (#211): restored env vars are NEVER emitted into this script.
// The script becomes an argv element (`bash -c "<script>"`), which is readable
// by any local user via `ps auxww` / `/proc/<pid>/cmdline`. Instead the restored
// vars are handed to the child through its ENVIRONMENT via `apply_restored_env`
// (portable_pty `CommandBuilder::env`), so secret values never touch a command line.
#[cfg(unix)]
fn build_wrapper_script(command: &str, state: &ShellState, base_cwd: &PathBuf) -> String {
    let effective_cwd = state.cwd.as_ref().unwrap_or(base_cwd);
    let cwd_escaped: String = effective_cwd.to_string_lossy().replace('\'', "'\\''");

    format!(
        r#"set -e
cd '{cwd}'
set +e
{user_cmd}
__CC_EXIT_CODE=$?
echo '{sentinel}'
pwd
env | grep -E '^[A-Za-z_][A-Za-z0-9_]*=' || true
exit $__CC_EXIT_CODE
"#,
        cwd = cwd_escaped,
        user_cmd = command,
        sentinel = SHELL_STATE_SENTINEL,
    )
}

/// Set [`CHILD_ENV`] on a PTY child, plus [`DEFAULT_TERM`] when it inherited no
/// `TERM` (#442).
#[cfg(unix)]
fn apply_child_env_defaults(cmd: &mut portable_pty::CommandBuilder) {
    for (key, value) in CHILD_ENV {
        cmd.env(key, value);
    }
    if cmd.get_env("TERM").is_none() {
        cmd.env("TERM", DEFAULT_TERM);
    }
}

/// Plumb the persisted shell env vars into the child process's environment
/// (NOT its argv). Each var is set with `CommandBuilder::env`, so the child
/// inherits it exactly as `export` would have — but the value never appears on
/// any command line. This is the sole path through which restored values reach
/// the child (#211).
#[cfg(unix)]
fn apply_restored_env(cmd: &mut portable_pty::CommandBuilder, env_vars: &HashMap<String, String>) {
    for (k, v) in env_vars {
        cmd.env(k, v);
    }
}

// ---------------------------------------------------------------------------
// Background execution (identical to bash.rs — no PTY needed for background)
// ---------------------------------------------------------------------------

async fn run_in_background(
    command: String,
    cwd: PathBuf,
    timeout_ms: u64,
    spill_dir: &Path,
) -> ToolResult {
    let task_name = format!("bg: {}", &command[..command.floor_char_boundary(60)]);
    let mut task = BackgroundTask::new(&task_name);
    task.pid = None;
    let task_id = global_registry().register(task);
    let task_id_clone = task_id.clone();
    let command_clone = command.clone();
    let spill_path = spill_dir.join(format!("{task_id}.txt"));
    let term = std::env::var_os("TERM").unwrap_or_else(|| DEFAULT_TERM.into());

    tokio::spawn(async move {
        let result = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
            // kill_on_drop: when the timeout drops this future the child must die
            // with it, otherwise a timed-out background command leaks (#220).
            let child = if cfg!(windows) {
                Command::new("cmd")
                    .arg("/C")
                    .arg(&command_clone)
                    .current_dir(&cwd)
                    .envs(CHILD_ENV)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .stdin(Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
            } else {
                Command::new("bash")
                    .arg("-c")
                    .arg(&command_clone)
                    .current_dir(&cwd)
                    .envs(CHILD_ENV)
                    .env("TERM", &term)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .stdin(Stdio::null())
                    .kill_on_drop(true)
                    .spawn()
            };

            match child {
                Ok(mut c) => {
                    if let Some(pid) = c.id() {
                        global_registry().set_pid(&task_id_clone, pid);
                    }
                    let mut stdout = c.stdout.take().map(|s| BufReader::new(s).lines());
                    let mut stderr = c.stderr.take().map(|s| BufReader::new(s).lines());
                    // Full output for the spill file; the registry keeps only
                    // bounded windows of lines (#442).
                    let mut full = OutputAccumulator::new(
                        RAW_HEAD_BYTES,
                        RAW_TAIL_BYTES,
                        Some(spill_path.clone()),
                    );
                    // Drain both pipes concurrently until both close. Reading
                    // stdout to EOF first let a child that filled the stderr pipe
                    // block forever (#442).
                    while stdout.is_some() || stderr.is_some() {
                        let line = tokio::select! {
                            line = next_line(&mut stdout), if stdout.is_some() => match line {
                                Some(line) => line,
                                None => {
                                    stdout = None;
                                    continue;
                                }
                            },
                            line = next_line(&mut stderr), if stderr.is_some() => match line {
                                Some(line) => format!("STDERR: {}", line),
                                None => {
                                    stderr = None;
                                    continue;
                                }
                            },
                        };
                        full.push(line.as_bytes());
                        full.push(b"\n");
                        global_registry().append_output(&task_id_clone, &line);
                    }
                    let spilled = match full.finish() {
                        Captured::Truncated { tail, spill, .. } => finish_spill(spill, &tail),
                        Captured::Complete(bytes) => {
                            match bound_head_tail(&String::from_utf8_lossy(&bytes)) {
                                Some(_) => spill_bytes(&spill_path, &bytes),
                                None => None,
                            }
                        }
                    };
                    if let Some(path) = spilled {
                        global_registry().set_output_file(&task_id_clone, path);
                    }
                    match c.wait().await {
                        Ok(status) if status.success() => {
                            global_registry().complete(&task_id_clone);
                        }
                        Ok(status) => {
                            let code = status.code().unwrap_or(-1);
                            global_registry().update_status(
                                &task_id_clone,
                                claurst_core::tasks::TaskStatus::Failed(format!(
                                    "exit code {}",
                                    code
                                )),
                            );
                        }
                        Err(e) => {
                            global_registry().update_status(
                                &task_id_clone,
                                claurst_core::tasks::TaskStatus::Failed(e.to_string()),
                            );
                        }
                    }
                }
                Err(e) => {
                    global_registry().update_status(
                        &task_id_clone,
                        claurst_core::tasks::TaskStatus::Failed(e.to_string()),
                    );
                }
            }
        })
        .await;

        if result.is_err() {
            global_registry().update_status(
                &task_id_clone,
                claurst_core::tasks::TaskStatus::Failed(format!(
                    "timed out after {}ms",
                    timeout_ms
                )),
            );
        }
    });

    ToolResult::success(format!(
        "Command started in background.\nTask ID: {}\nCommand: {}",
        task_id, command
    ))
}

/// Next line from an optional line reader; `None` at EOF, on a read error, or
/// when the reader is absent.
async fn next_line<R: tokio::io::AsyncBufRead + Unpin>(
    lines: &mut Option<tokio::io::Lines<R>>,
) -> Option<String> {
    match lines {
        Some(lines) => lines.next_line().await.ok().flatten(),
        None => None,
    }
}

// ---------------------------------------------------------------------------
// ANSI stripping (Unix only — PTY output only happens on Unix)
// ---------------------------------------------------------------------------

/// Remove ANSI/VT escape sequences from PTY output, producing clean text.
#[cfg(unix)]
fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next(); // consume '['
                    // CSI: consume parameter + intermediate bytes, stop at final byte
                    for c in &mut chars {
                        if c.is_ascii_alphabetic() || c == '@' {
                            break;
                        }
                    }
                }
                Some(']') => {
                    // OSC: consume until ST (ESC \) or BEL
                    chars.next(); // consume ']'
                    let mut prev = '\0';
                    for c in &mut chars {
                        if c == '\x07' {
                            break; // BEL terminates OSC
                        }
                        if prev == '\x1b' && c == '\\' {
                            break; // ST = ESC \ terminates OSC
                        }
                        prev = c;
                    }
                }
                Some('(') | Some(')') | Some('*') | Some('+') => {
                    chars.next(); // consume designator introducer
                    chars.next(); // consume charset code
                }
                _ => {
                    // Two-character escape (ESC X): skip next char
                    chars.next();
                }
            }
        } else if ch == '\r' {
            // CR without LF: treat as line reset (discard pending partial line)
            // CR+LF is fine: LF will follow and push the newline
        } else {
            result.push(ch);
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Unix PTY execution
// ---------------------------------------------------------------------------

/// Outcome of a single foreground PTY run.
#[cfg(unix)]
enum PtyOutcome {
    /// The command finished; carries (bounded raw PTY output, exit code).
    Completed(Captured, i32),
    /// The command exceeded its timeout. The child has been KILLED (#220).
    TimedOut,
    /// The PTY could not be set up / the command could not be spawned.
    Failed(String),
}

/// Guard that guarantees the PTY child is killed if the running future is
/// dropped before the command completes — e.g. the turn is cancelled or the
/// task is aborted mid-command. This is the PTY analogue of tokio's
/// `kill_on_drop(true)`: portable_pty's child is NOT killed on drop, so without
/// this a cancelled command would orphan its child (#220).
///
/// The guard is disarmed on normal completion and fired explicitly on timeout.
#[cfg(unix)]
struct PtyKillGuard {
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    armed: bool,
}

#[cfg(unix)]
impl PtyKillGuard {
    fn new(killer: Box<dyn portable_pty::ChildKiller + Send + Sync>) -> Self {
        Self { killer, armed: true }
    }

    /// The command finished on its own — there is nothing left to kill.
    fn disarm(&mut self) {
        self.armed = false;
    }

    /// Kill the child now (timeout path) and disarm so `Drop` is a no-op.
    fn kill_now(&mut self) {
        if self.armed {
            let _ = self.killer.kill();
            self.armed = false;
        }
    }
}

#[cfg(unix)]
impl Drop for PtyKillGuard {
    fn drop(&mut self) {
        if self.armed {
            // Future dropped before completion (cancel / abort): don't leak the child.
            let _ = self.killer.kill();
        }
    }
}

#[cfg(unix)]
async fn run_in_pty(
    script: &str,
    working_dir: &str,
    env_vars: &HashMap<String, String>,
    timeout: Duration,
    spill_path: PathBuf,
) -> PtyOutcome {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    let pty_system = native_pty_system();

    let pair = match pty_system.openpty(PtySize {
        rows: 50,
        cols: 220,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => return PtyOutcome::Failed(format!("Failed to open PTY: {}", e)),
    };

    let mut cmd = CommandBuilder::new("bash");
    cmd.args(["-c", script]);
    cmd.cwd(working_dir);
    apply_child_env_defaults(&mut cmd);
    // Restored shell vars go through the child's ENVIRONMENT, never its argv (#211).
    apply_restored_env(&mut cmd, env_vars);

    let child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => return PtyOutcome::Failed(format!("Failed to spawn in PTY: {}", e)),
    };

    // Grab the reader *before* dropping slave so the fd stays valid.
    let reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => return PtyOutcome::Failed(format!("Failed to clone PTY reader: {}", e)),
    };

    // A killer handle cloned from the child. This is how a timed-out or
    // cancelled command is explicitly KILLED (portable_pty does not kill on
    // drop, so without this the child would orphan) (#220).
    let killer = child.clone_killer();

    // Drop slave after spawn — once the child's controlling terminal is gone,
    // the master side will see EOF when the child exits.
    drop(pair.slave);
    // Keep master alive (inside the read thread) until reading is done.
    let master = pair.master;

    // Read all PTY output in a blocking thread (portable_pty reader is sync) and
    // reap the child there so we can return its exit code. drive_pty_child stops
    // reading as soon as the DIRECT child exits rather than waiting for pty EOF,
    // which a detached grandchild would hold open forever (#184).
    let read_handle = tokio::task::spawn_blocking(move || {
        let master_fd = master.as_raw_fd();
        let result = drive_pty_child(child, reader, master_fd, spill_path);
        // Keep the master fd alive until reading + reaping is complete.
        drop(master);
        result
    });

    // Kill-on-drop guard: if this future is dropped mid-command (turn cancelled)
    // the guard fires and kills the child instead of leaking it (#220).
    let mut guard = PtyKillGuard::new(killer);

    match tokio::time::timeout(timeout, read_handle).await {
        Ok(Ok((captured, exit_code))) => {
            guard.disarm();
            PtyOutcome::Completed(captured, exit_code)
        }
        Ok(Err(e)) => {
            guard.disarm();
            PtyOutcome::Failed(format!("PTY read thread panicked: {}", e))
        }
        Err(_) => {
            // Timed out: explicitly KILL the child so it can't linger as an
            // orphan (#220). The read thread then observes the exit and finishes.
            guard.kill_now();
            PtyOutcome::TimedOut
        }
    }
}

/// Poll interval used to wake the PTY read loop so it can notice that the direct
/// child has exited even while a detached grandchild still holds the pty open.
#[cfg(unix)]
const PTY_POLL_INTERVAL_MS: i32 = 20;

/// Wait up to `timeout_ms` for `fd` to become readable. Returns `true` when the
/// caller should attempt a read — data ready, EOF/hangup, or a poll error we'd
/// rather surface through `read` — and `false` on a clean timeout with nothing
/// pending.
#[cfg(unix)]
fn poll_readable(fd: std::os::unix::io::RawFd, timeout_ms: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let rc = unsafe { libc::poll(&mut pfd as *mut libc::pollfd, 1, timeout_ms) };
        if rc < 0 {
            // Retry on EINTR; on any other error let the subsequent `read` surface it.
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return true;
        }
        // rc == 0 → clean timeout, nothing ready. rc > 0 → POLLIN/POLLHUP/POLLERR.
        return rc > 0;
    }
}

/// Drive a spawned PTY child to completion: read its output while it runs, reap
/// it, and return `(captured output, exit_code)`.
///
/// The pty is drained to the end however much the child writes: output past
/// the in-memory head + tail windows is dropped from the middle rather than
/// left unread, which would block the child on a full pty until timeout (#442).
///
/// Crucially — and mirroring bash.rs's `drive_child` — the loop stops as soon as
/// the DIRECT child exits rather than waiting for pty EOF. A grandchild that
/// detached into its own session (`setsid`) inherits the pty slave and would
/// otherwise hold it open indefinitely, hanging the tool for the grandchild's
/// full lifetime (#184). `poll_readable` lets the blocking read wake up
/// periodically so we can re-check `try_wait()` without spinning.
#[cfg(unix)]
fn drive_pty_child(
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    mut reader: Box<dyn std::io::Read + Send>,
    master_fd: Option<std::os::unix::io::RawFd>,
    spill_path: PathBuf,
) -> (Captured, i32) {
    use std::io::Read;

    let mut output = OutputAccumulator::new(RAW_HEAD_BYTES, RAW_TAIL_BYTES, Some(spill_path));
    let mut buf = vec![0u8; 64 * 1024];

    // Set once the direct child has exited. We then drain any already-buffered
    // pty output and stop — we never block waiting for EOF that a detached
    // grandchild would hold open forever (#184).
    let mut child_exited = false;

    loop {
        // Wait briefly for data. When we can poll, this lets the loop wake up to
        // re-check the child's liveness even while the pty is held open elsewhere.
        let data_ready = match master_fd {
            Some(fd) => poll_readable(fd, PTY_POLL_INTERVAL_MS),
            None => true, // can't poll — fall back to a plain blocking read
        };

        if data_ready {
            match reader.read(&mut buf) {
                Ok(0) => break, // EOF: every pty writer (incl. the child) is gone
                Ok(n) => {
                    output.push(&buf[..n]);
                    continue; // keep draining while bytes remain
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
        }

        // Nothing was ready this cycle.
        if child_exited {
            // Direct child already exited and the pty is now idle → stop instead
            // of hanging on a detached grandchild still holding it open (#184).
            break;
        }
        match child.try_wait() {
            Ok(Some(_)) => {
                // Direct child is done. Loop once more to drain any final buffered
                // bytes; the `child_exited` guard above then breaks us out.
                child_exited = true;
            }
            Ok(None) => {}   // still running
            Err(_) => break, // can't observe the child — bail rather than spin
        }
    }

    let exit_code = match child.wait() {
        Ok(status) => status.exit_code() as i32,
        Err(_) => -1,
    };
    (output.finish(), exit_code)
}

/// Split raw PTY output at the wrapper's shell-state sentinel: returns the
/// user's output and the state lines that follow the sentinel (`pwd`, then the
/// `env` dump). The sentinel must sit on a line of its own; the last such line
/// wins. Works on raw bytes so it also applies to a retained tail (#442).
#[cfg(unix)]
fn split_state_block(raw: &[u8]) -> (&[u8], Vec<String>) {
    let needle = SHELL_STATE_SENTINEL.as_bytes();
    let mut end = raw.len();
    while let Some(pos) = raw[..end].windows(needle.len()).rposition(|w| w == needle) {
        let rest = &raw[pos + needle.len()..];
        let own_line = (pos == 0 || raw[pos - 1] == b'\n')
            && (rest.is_empty() || rest.starts_with(b"\n") || rest.starts_with(b"\r\n"));
        if own_line {
            let state = strip_ansi(&String::from_utf8_lossy(rest));
            // `rest` opens with the sentinel line's own line ending; skip it.
            return (&raw[..pos], state.lines().skip(1).map(str::to_string).collect());
        }
        end = pos + needle.len() - 1;
    }
    (raw, Vec::new())
}

/// Turn captured PTY output into the text shown to the model plus the shell
/// state lines. ANSI codes are stripped and the final newline dropped; output
/// larger than the head + tail windows keeps only those windows, and the full
/// output is spilled to `spill_path` with a `Full output:` trailer (#442). The
/// shell-state block (it carries the environment) is never written to disk.
#[cfg(unix)]
fn render_pty_output(captured: Captured, spill_path: &Path) -> (String, Vec<String>) {
    match captured {
        Captured::Complete(raw) => {
            let (user, state) = split_state_block(&raw);
            let text = strip_ansi(&String::from_utf8_lossy(user));
            let text = text.strip_suffix('\n').unwrap_or(&text);
            let output = match bound_head_tail(text) {
                Some(bounded) => with_full_output(bounded, spill_bytes(spill_path, text.as_bytes())),
                None => text.to_string(),
            };
            (output, state)
        }
        Captured::Truncated {
            head,
            tail,
            omitted,
            spill,
        } => {
            let (tail, state) = split_state_block(&tail);
            // The stream overflowed memory: the spill file holds the raw PTY
            // bytes (head + dropped middle); finish it with the user's tail.
            let spilled = finish_spill(spill, tail);
            // The rolling tail starts mid-line (possibly mid-char or mid-escape):
            // begin at the next full line.
            let tail = match tail.iter().position(|&b| b == b'\n') {
                Some(i) => &tail[i + 1..],
                None => tail,
            };
            let head_text = strip_ansi(&String::from_utf8_lossy(&head));
            let tail_text = strip_ansi(&String::from_utf8_lossy(tail));
            let tail_text = tail_text.strip_suffix('\n').unwrap_or(&tail_text);
            let head = head_window(&head_text, OUTPUT_WINDOW_LINES, OUTPUT_WINDOW_BYTES);
            let tail = tail_window(tail_text, OUTPUT_WINDOW_LINES, OUTPUT_WINDOW_BYTES);
            let omitted =
                omitted + (head_text.len() - head.len() + tail_text.len() - tail.len()) as u64;
            let output = join_head_tail(head, tail, &format!("about {omitted} bytes"));
            (with_full_output(output, spilled), state)
        }
    }
}

// ---------------------------------------------------------------------------
// Windows fallback (cmd.exe, no PTY)
// ---------------------------------------------------------------------------

/// Windows counterpart of `PtyKillGuard` (#391). `tokio::process::Child` is not
/// killed on drop, and even `kill_on_drop(true)` / `Child::kill` only terminate
/// `cmd.exe` itself — its children (e.g. `python.exe`) would keep running. If
/// the future driving `run_windows_fallback` is dropped mid-command (turn
/// cancelled / task aborted), this guard kills the whole process tree with
/// `taskkill /T /F`.
///
/// The guard is disarmed on normal completion and fired explicitly on timeout.
#[cfg(windows)]
struct WindowsProcessTreeGuard {
    /// Root PID of the tree; `None` when there is nothing (left) to kill.
    pid: Option<u32>,
}

#[cfg(windows)]
impl WindowsProcessTreeGuard {
    fn new(pid: Option<u32>) -> Self {
        Self { pid }
    }

    /// The command finished on its own — there is nothing left to kill.
    fn disarm(&mut self) {
        self.pid = None;
    }

    /// Kill the process tree now (timeout path) and disarm so `Drop` is a no-op.
    fn kill_now(&mut self) {
        if let Some(pid) = self.pid.take() {
            kill_process_tree(pid);
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsProcessTreeGuard {
    fn drop(&mut self) {
        // Future dropped before completion (cancel / abort): don't leak the tree.
        self.kill_now();
    }
}

/// Forcefully terminate `pid` and every descendant. Runs synchronously so the
/// tree is gone by the time the guard's drop returns; failures are ignored
/// (the process may already have exited).
#[cfg(windows)]
fn kill_process_tree(pid: u32) {
    use std::os::windows::process::CommandExt;
    // Keep taskkill from flashing a console window when the host is a GUI app.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status();
}

#[cfg(windows)]
async fn run_windows_fallback(
    command: &str,
    effective_cwd: &PathBuf,
    timeout_dur: Duration,
    timeout_ms: u64,
    spill_dir: &Path,
) -> ToolResult {
    let mut child = match Command::new("cmd")
        .arg("/C")
        .arg(command)
        .current_dir(effective_cwd)
        .envs(CHILD_ENV)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return ToolResult::error(format!("Failed to spawn command: {}", e)),
    };

    // Kill-on-drop guard: if this future is dropped mid-command (turn cancelled)
    // the guard fires and kills cmd.exe and all of its children (#391).
    let mut guard = WindowsProcessTreeGuard::new(child.id());

    let stdout_handle = child.stdout.take();
    let stderr_handle = child.stderr.take();

    let result = tokio::time::timeout(timeout_dur, async {
        let mut stdout_lines = Vec::new();
        let mut stderr_lines = Vec::new();

        if let Some(stdout) = stdout_handle {
            let reader = BufReader::new(stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                stdout_lines.push(line);
            }
        }
        if let Some(stderr) = stderr_handle {
            let reader = BufReader::new(stderr);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                stderr_lines.push(line);
            }
        }
        let status = child.wait().await;
        (stdout_lines, stderr_lines, status)
    })
    .await;

    match result {
        Ok((stdout_lines, stderr_lines, status)) => {
            guard.disarm();
            let exit_code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
            let mut output = String::new();
            if !stdout_lines.is_empty() {
                output.push_str(&stdout_lines.join("\n"));
            }
            if !stderr_lines.is_empty() {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str("STDERR:\n");
                output.push_str(&stderr_lines.join("\n"));
            }
            if output.is_empty() {
                output = "(no output)".to_string();
            }
            let output = match bound_head_tail(&output) {
                Some(bounded) => {
                    let path = spill_dir.join(format!("{}.txt", uuid::Uuid::new_v4()));
                    with_full_output(bounded, spill_bytes(&path, output.as_bytes()))
                }
                None => output,
            };
            shell_result(output, exit_code)
        }
        Err(_) => {
            // Kill the whole tree, not just cmd.exe (#391); `child.kill` remains
            // as a fallback should taskkill be unavailable, and reaps the child.
            guard.kill_now();
            let _ = child.kill().await;
            ToolResult::error(format!("Command timed out after {}ms", timeout_ms))
        }
    }
}

// ---------------------------------------------------------------------------
// Shared result helper
// ---------------------------------------------------------------------------

/// Wrap already-bounded command output in a tool result keyed on the exit code.
fn shell_result(output: String, exit_code: i32) -> ToolResult {
    if exit_code != 0 {
        ToolResult::error(format!("Command exited with code {}\n{}", exit_code, output))
    } else {
        ToolResult::success(output)
    }
}

// ---------------------------------------------------------------------------
// Tool implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Tool for PtyBashTool {
    // Gates itself: calls `ctx.check_permission*` in `execute()` (#210).
    fn self_gates(&self) -> bool { true }

    fn name(&self) -> &str {
        claurst_core::constants::TOOL_NAME_BASH
    }

    fn description(&self) -> &str {
        "Executes a given bash command in a real terminal (PTY) and returns its output. \
         The working directory persists between commands. Supports interactive programs, \
         colored output (stripped for readability), and terminal-aware tools like npm, \
         cargo, git, and pytest. Use for running shell commands, scripts, git operations, \
         and system tasks."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Execute
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The bash command to execute"
                },
                "description": {
                    "type": "string",
                    "description": "Clear, concise description of what this command does"
                },
                "timeout": {
                    "type": "number",
                    "description": "Optional timeout in milliseconds (max 600000, default 120000)"
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "Set to true to run command in the background"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        let params: BashInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        // Permission check
        let reason = params
            .description
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("This will execute a shell command.")
            .to_string();

        if let Err(e) = ctx.check_permission_for_path(
            self.name(),
            &reason,
            std::path::PathBuf::from(&params.command),
            false,
        ) {
            return ToolResult::error(e.to_string());
        }

        // Security classifier — block Critical-risk commands unconditionally.
        if classify_bash_command(&params.command) == BashRiskLevel::Critical {
            return ToolResult::error(format!(
                "Command blocked: classified as Critical risk by the bash security classifier.\n\
                 Refusing to execute: {}",
                params.command
            ));
        }

        let timeout_ms = params.timeout.min(600_000);
        let timeout_dur = Duration::from_millis(timeout_ms);
        let shell_state_arc = session_shell_state(&ctx.session_id);
        let spill_dir = claurst_core::paths::tool_output_dir();

        // ── Background path ──────────────────────────────────────────────────
        if params.run_in_background {
            let cwd = {
                let state = shell_state_arc.lock();
                state.cwd.clone().unwrap_or_else(|| ctx.working_dir.clone())
            };
            return run_in_background(params.command, cwd, timeout_ms, &spill_dir).await;
        }

        debug!(command = %params.command, "Executing bash command via PTY");

        // ── Windows path (no PTY — use cmd.exe fallback) ─────────────────────
        #[cfg(windows)]
        {
            let effective_cwd = {
                let state = shell_state_arc.lock();
                state.cwd.clone().unwrap_or_else(|| ctx.working_dir.clone())
            };
            return run_windows_fallback(
                &params.command,
                &effective_cwd,
                timeout_dur,
                timeout_ms,
                &spill_dir,
            )
            .await;
        }

        // ── Unix PTY path ────────────────────────────────────────────────────
        #[cfg(unix)]
        {
            run_pty_foreground(
                &params.command,
                &shell_state_arc,
                &ctx.working_dir,
                timeout_dur,
                timeout_ms,
                &spill_dir,
            )
            .await
        }
    }
}

/// Run `command` in a PTY with the session's persisted shell state, fold the
/// wrapper's state block back into that state, and render the bounded result.
#[cfg(unix)]
async fn run_pty_foreground(
    command: &str,
    shell_state: &parking_lot::Mutex<ShellState>,
    base_cwd: &PathBuf,
    timeout_dur: Duration,
    timeout_ms: u64,
    spill_dir: &Path,
) -> ToolResult {
    // Build the wrapper script that restores cwd + captures shell state.
    // Restored env vars are cloned out and later injected into the child's
    // environment (never its argv) — see `apply_restored_env` (#211).
    let (script, restored_env, working_dir_str) = {
        let state = shell_state.lock();
        let script = build_wrapper_script(command, &state, base_cwd);
        let restored_env = state.env_vars.clone();
        let wd = base_cwd.to_string_lossy().into_owned();
        (script, restored_env, wd)
    };

    // run_in_pty owns the timeout so it can KILL the child when it fires
    // (a bare outer timeout would just drop the future and orphan it) (#220).
    let spill_path = spill_dir.join(format!("{}.txt", uuid::Uuid::new_v4()));
    let outcome =
        run_in_pty(&script, &working_dir_str, &restored_env, timeout_dur, spill_path.clone())
            .await;

    match outcome {
        PtyOutcome::Completed(captured, exit_code) => {
            let (mut output, state_lines) = render_pty_output(captured, &spill_path);

            // Update persistent shell state
            if let Some((new_cwd, env_delta)) = parse_shell_state_block(&state_lines) {
                let mut state = shell_state.lock();
                state.cwd = Some(new_cwd);
                for (k, v) in env_delta {
                    state.env_vars.insert(k, v);
                }
            }

            // Fast-path export capture
            let exports = extract_exports_from_command(command);
            if !exports.is_empty() {
                let mut state = shell_state.lock();
                for (k, v) in exports {
                    state.env_vars.insert(k, v);
                }
            }

            if output.is_empty() {
                output = "(no output)".to_string();
            }
            shell_result(output, exit_code)
        }
        PtyOutcome::Failed(e) => ToolResult::error(format!("PTY execution failed: {}", e)),
        PtyOutcome::TimedOut => {
            ToolResult::error(format!("Command timed out after {}ms", timeout_ms))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    /// #211: restored env values (secrets) must NOT be baked into the wrapper
    /// script, because that script becomes an argv element of `bash -c`, which is
    /// visible to any local user via `ps auxww` / `/proc/<pid>/cmdline`.
    #[test]
    fn wrapper_script_never_embeds_restored_env_values() {
        let mut state = ShellState::new();
        state
            .env_vars
            .insert("SECRET".to_string(), "topsecret".to_string());
        state
            .env_vars
            .insert("AWS_SECRET_ACCESS_KEY".to_string(), "aws-argv-leak".to_string());

        let base = PathBuf::from("/tmp");
        let script = build_wrapper_script("echo hi", &state, &base);

        // The secret VALUES must never appear in the script string / argv.
        assert!(
            !script.contains("topsecret"),
            "secret value leaked into wrapper script (argv):\n{script}"
        );
        assert!(
            !script.contains("aws-argv-leak"),
            "AWS secret leaked into wrapper script (argv):\n{script}"
        );
        // And there must be no re-exported `export KEY=` lines for restored vars.
        assert!(
            !script.contains("export SECRET="),
            "restored var was re-exported into the script (argv):\n{script}"
        );
        assert!(
            !script.contains("export AWS_SECRET_ACCESS_KEY="),
            "restored var was re-exported into the script (argv):\n{script}"
        );
    }

    /// #211: the value IS delivered to the child — but through its environment
    /// (`CommandBuilder::env`), which is not part of argv. This exercises the
    /// env-plumbing function directly.
    #[test]
    fn restored_env_reaches_child_via_env_map_not_argv() {
        use portable_pty::CommandBuilder;

        let mut env_vars = HashMap::new();
        env_vars.insert("SECRET".to_string(), "topsecret".to_string());

        let mut cmd = CommandBuilder::new("bash");
        cmd.args(["-c", "true"]);
        apply_restored_env(&mut cmd, &env_vars);

        // Value is present in the child's environment...
        assert_eq!(cmd.get_env("SECRET"), Some(OsStr::new("topsecret")));
        // ...and NOT in argv (only `bash -c true`).
        for arg in cmd.get_argv() {
            assert!(
                !arg.to_string_lossy().contains("topsecret"),
                "secret value leaked into CommandBuilder argv: {arg:?}"
            );
        }
    }

    /// #442: output whose cut point lands inside a multibyte char used to panic
    /// on a byte-offset slice. 'é' is 2 bytes; the leading ASCII byte puts every
    /// char boundary on an odd offset, so the even-sized window cuts split one.
    #[test]
    fn render_multibyte_at_cut_does_not_panic() {
        let dir = tempfile::tempdir().unwrap();
        let spill = dir.path().join("out.txt");
        let text = format!("a{}", "é".repeat(60_000));
        let (output, state) =
            render_pty_output(Captured::Complete(text.clone().into_bytes()), &spill);
        assert!(state.is_empty());
        assert!(output.contains("bytes omitted"), "len {}", output.len());
        assert!(output.starts_with('a'));
        assert!(output.ends_with(&format!("é\n\nFull output: {}", spill.display())));
        assert_eq!(std::fs::read_to_string(&spill).unwrap(), text);
    }

    #[test]
    fn render_splits_state_block_from_complete_output() {
        let dir = tempfile::tempdir().unwrap();
        let spill = dir.path().join("out.txt");
        let raw = format!("hello\r\nworld\r\n{SHELL_STATE_SENTINEL}\r\n/tmp\r\nFOO=bar\r\n");
        let (output, state) = render_pty_output(Captured::Complete(raw.into_bytes()), &spill);
        assert_eq!(output, "hello\nworld");
        assert!(!spill.exists(), "output that fits must not spill");
        assert_eq!(state, vec!["/tmp".to_string(), "FOO=bar".to_string()]);
    }

    /// A sentinel embedded in a line of user output is not the state block.
    #[test]
    fn render_ignores_sentinel_inside_a_line() {
        let dir = tempfile::tempdir().unwrap();
        let raw = format!("x{SHELL_STATE_SENTINEL}\n");
        let (output, state) =
            render_pty_output(Captured::Complete(raw.into_bytes()), &dir.path().join("o.txt"));
        assert_eq!(output, format!("x{SHELL_STATE_SENTINEL}"));
        assert!(state.is_empty());
    }

    #[test]
    fn render_truncated_keeps_head_tail_and_state() {
        let tail = format!("partial\nlast line\n{SHELL_STATE_SENTINEL}\n/tmp\n");
        let dir = tempfile::tempdir().unwrap();
        let captured = Captured::Truncated {
            head: b"first line\n".to_vec(),
            tail: tail.into_bytes(),
            omitted: 1000,
            spill: None,
        };
        let (output, state) = render_pty_output(captured, &dir.path().join("o.txt"));
        assert!(output.starts_with("first line"));
        assert!(output.ends_with("last line"));
        assert!(!output.contains("partial"));
        assert!(output.contains("bytes omitted"));
        assert_eq!(state, vec!["/tmp".to_string()]);
    }

    /// #442: TERM is filled in only when the child would otherwise have none.
    #[test]
    fn child_env_defaults_keep_an_inherited_term() {
        use portable_pty::CommandBuilder;

        let mut cmd = CommandBuilder::new("bash");
        cmd.env_remove("TERM");
        apply_child_env_defaults(&mut cmd);
        assert_eq!(cmd.get_env("TERM"), Some(OsStr::new(DEFAULT_TERM)));
        assert_eq!(cmd.get_env("GIT_PAGER"), Some(OsStr::new("cat")));

        let mut cmd = CommandBuilder::new("bash");
        cmd.env("TERM", "xterm-256color");
        apply_child_env_defaults(&mut cmd);
        assert_eq!(cmd.get_env("TERM"), Some(OsStr::new("xterm-256color")));
    }

    // -----------------------------------------------------------------------
    // Execution tests (#220 / #184) — exercise the live PTY path end-to-end.
    // -----------------------------------------------------------------------

    /// Permission handler that allows everything — for exercising `execute`.
    struct AllowAllHandler;

    impl claurst_core::permissions::PermissionHandler for AllowAllHandler {
        fn check_permission(
            &self,
            _request: &claurst_core::permissions::PermissionRequest,
        ) -> claurst_core::permissions::PermissionDecision {
            claurst_core::permissions::PermissionDecision::Allow
        }

        fn request_permission(
            &self,
            _request: &claurst_core::permissions::PermissionRequest,
        ) -> claurst_core::permissions::PermissionDecision {
            claurst_core::permissions::PermissionDecision::Allow
        }
    }

    fn allow_all_context() -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            permission_mode: claurst_core::config::PermissionMode::Default,
            permission_handler: std::sync::Arc::new(AllowAllHandler),
            cost_tracker: claurst_core::cost::CostTracker::new(),
            session_id: "pty-bash-test".to_string(),
            file_history: std::sync::Arc::new(parking_lot::Mutex::new(
                claurst_core::file_history::FileHistory::new(),
            )),
            current_turn: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            non_interactive: true,
            mcp_manager: None,
            config: claurst_core::config::Config::default(),
            managed_agent_config: None,
            completion_notifier: None,
            pending_permissions: None,
            permission_manager: None,
            user_question_tx: None,
            cancel_token: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// #220: a command that exceeds its timeout must have its child KILLED, not
    /// merely dropped. portable_pty does not kill the child on drop, so before
    /// the fix a timed-out command left an orphaned process running to its
    /// natural end. We time out a long `sleep` and then assert the process is
    /// gone.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn timeout_kills_the_child() {
        let tool = PtyBashTool;
        let ctx = allow_all_context();

        // A distinctive duration doubles as a searchable marker for the process.
        let marker = "sleep 31337";
        let input = json!({
            "command": marker,
            "timeout": 500u64, // ms — far shorter than the sleep
        });

        let started = std::time::Instant::now();
        let result = tool.execute(input, &ctx).await;
        let elapsed = started.elapsed();

        assert!(
            result.is_error,
            "a timed-out command should be an error, got: {}",
            result.content
        );
        assert!(
            result.content.contains("timed out"),
            "expected a timeout message, got: {}",
            result.content
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "execute should return shortly after the 500ms timeout, took {:?}",
            elapsed
        );

        // Give the kill a moment to propagate, then assert the sleep is gone.
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Ok(out) = std::process::Command::new("pgrep")
            .arg("-f")
            .arg(marker)
            .output()
        {
            let pids = String::from_utf8_lossy(&out.stdout);
            let lingering: Vec<&str> = pids.split_whitespace().collect();
            assert!(
                lingering.is_empty(),
                "timed-out child `{marker}` was not killed; still running (pids: {lingering:?})",
            );
        }
    }

    /// #442: 10 MB of output used to hit the reader's 2 MB head-only cap, after
    /// which nothing drained the pty, the child blocked, and the call timed out.
    /// It must now complete with a head window, a tail window, a private spill
    /// file holding all of it, and the shell state (cwd) still captured from
    /// the end of the stream -- without writing that state (env) to disk.
    #[tokio::test]
    async fn ten_megabytes_of_output_completes_with_head_tail_and_spill() {
        let state = parking_lot::Mutex::new(ShellState::new());
        let base = std::env::temp_dir();
        let spill_dir = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();
        let result = run_pty_foreground(
            "cd / && yes line | head -n 2000000; echo END",
            &state,
            &base,
            Duration::from_secs(120),
            120_000,
            spill_dir.path(),
        )
        .await;
        assert!(
            !result.is_error,
            "command failed after {:?}: {}",
            started.elapsed(),
            &result.content[..result.content.len().min(200)]
        );
        assert!(result.content.starts_with("line\nline\n"));
        assert!(result.content.contains("line\nEND\n\nFull output: "));
        assert!(result.content.contains("bytes omitted"));
        assert!(
            result.content.len() <= 2 * OUTPUT_WINDOW_BYTES + 400,
            "result not bounded: {} bytes",
            result.content.len()
        );
        assert_eq!(state.lock().cwd, Some(PathBuf::from("/")));

        let (_, path) = result.content.rsplit_once("Full output: ").unwrap();
        let path = PathBuf::from(path);
        assert_eq!(path.parent(), Some(spill_dir.path()));
        let spilled = std::fs::read(&path).unwrap();
        // 2M "line" rows; the pty turns each "\n" into "\r\n".
        assert_eq!(spilled.len(), 2_000_000 * 6 + "END\r\n".len());
        assert!(spilled.ends_with(b"line\r\nEND\r\n"));
        assert!(
            !spilled.windows(SHELL_STATE_SENTINEL.len()).any(|w| w == SHELL_STATE_SENTINEL.as_bytes()),
            "shell-state block (env) leaked into the spill file"
        );
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    /// Start `command` in the background and wait for its task to finish.
    async fn run_background_to_completion(command: &str, spill_dir: &Path) -> BackgroundTask {
        let started =
            run_in_background(command.to_string(), std::env::temp_dir(), 60_000, spill_dir).await;
        let task_id = started
            .content
            .lines()
            .find_map(|l| l.strip_prefix("Task ID: "))
            .unwrap()
            .to_string();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            let task = global_registry().get(&task_id).unwrap();
            if !task.is_running() {
                return task;
            }
            assert!(std::time::Instant::now() < deadline, "background task never finished");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// #442 (Monitor path): a chatty background task keeps bounded windows in
    /// the registry and spills its full output to a private file.
    #[tokio::test]
    async fn background_output_is_bounded_and_spilled() {
        let spill_dir = tempfile::tempdir().unwrap();
        let task = run_background_to_completion("seq 1 300000", spill_dir.path()).await;
        assert_eq!(task.status, claurst_core::tasks::TaskStatus::Completed);
        assert_eq!(task.output_line_count(), 300_000);

        let rendered = task.render_output();
        assert!(rendered.starts_with("1\n2\n"));
        assert!(rendered.contains("lines omitted"));
        let path = task.output_file.clone().unwrap();
        assert!(rendered.ends_with(&format!("300000\n\nFull output: {}", path.display())));

        let expected: String = (1..=300_000).map(|i| format!("{i}\n")).collect();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), expected);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    /// #442: stdout used to be read to EOF before stderr, so a child that
    /// filled the stderr pipe blocked forever. Both pipes now drain together.
    #[tokio::test]
    async fn background_drains_stderr_while_stdout_is_open() {
        let spill_dir = tempfile::tempdir().unwrap();
        let task = run_background_to_completion(
            "head -c 300000 /dev/zero | tr '\\0' x >&2; echo done",
            spill_dir.path(),
        )
        .await;
        assert_eq!(task.status, claurst_core::tasks::TaskStatus::Completed);
        assert!(task.output.iter().any(|l| l == "done"));
    }

    const ENV_PROBE: &str = "echo \"$PAGER $GIT_PAGER $GH_PAGER $GIT_TERMINAL_PROMPT $CLAURST $AGENT ${TERM:+term}\"";

    /// #442: the PTY child runs with pagers off, no git prompts, the agent
    /// markers, and a TERM.
    #[tokio::test]
    async fn pty_child_gets_pager_free_agent_env() {
        let state = parking_lot::Mutex::new(ShellState::new());
        let spill_dir = tempfile::tempdir().unwrap();
        let result = run_pty_foreground(
            ENV_PROBE,
            &state,
            &std::env::temp_dir(),
            Duration::from_secs(30),
            30_000,
            spill_dir.path(),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "cat cat cat 0 1 1 term");
    }

    /// #442: background children get the same environment.
    #[tokio::test]
    async fn background_child_gets_pager_free_agent_env() {
        let spill_dir = tempfile::tempdir().unwrap();
        let task = run_background_to_completion(ENV_PROBE, spill_dir.path()).await;
        assert_eq!(task.output, vec!["cat cat cat 0 1 1 term".to_string()]);
    }

    /// Regression test for #184: a tool command that spawns a child which
    /// detaches into its own session (`setsid`) inherits the pty slave. The
    /// read loop must stop once the *direct* child exits instead of blocking on
    /// pty EOF held open by the detached grandchild — otherwise the tool (and the
    /// agent turn) hangs for the grandchild's full lifetime.
    #[cfg(not(windows))]
    #[tokio::test]
    async fn foreground_does_not_hang_on_detached_grandchild() {
        let tool = PtyBashTool;
        let ctx = allow_all_context();

        // `setsid sleep 30` runs in a brand-new session but inherits the pty;
        // `&` lets the wrapper shell return immediately without waiting.
        let input = json!({
            "command": "setsid sleep 30 & echo spawned",
            "timeout": 120000u64,
        });

        let started = std::time::Instant::now();
        let result =
            tokio::time::timeout(Duration::from_secs(10), tool.execute(input, &ctx)).await;
        let elapsed = started.elapsed();

        let result = result.expect(
            "execute hung waiting on the detached grandchild's pty (regression of #184)",
        );
        assert!(
            !result.is_error,
            "command should have succeeded, got: {}",
            result.content
        );
        assert!(
            result.content.contains("spawned"),
            "expected the parent command's output, got: {}",
            result.content
        );
        assert!(
            elapsed < Duration::from_secs(10),
            "execute should return promptly after the direct child exits, took {:?}",
            elapsed
        );
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use base64::Engine as _;

    /// Distinctive ping count: doubles as a searchable command-line marker and
    /// keeps the command running far longer than the test.
    const MARKER: &str = "31391 127.0.0.1";

    /// PIDs of `cmd.exe` / `PING.EXE` processes whose command line carries
    /// `MARKER`. Queried through WMI via PowerShell; the script is passed with
    /// `-EncodedCommand` to sidestep Windows argument-quoting rules.
    fn marker_pids() -> Vec<u32> {
        let script = format!(
            "(Get-CimInstance Win32_Process -Filter \"(Name='PING.EXE' OR Name='cmd.exe') \
             AND CommandLine LIKE '%{MARKER}%'\").ProcessId"
        );
        let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(utf16);
        let out = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &encoded])
            .output()
            .expect("failed to run powershell");
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .filter_map(|p| p.parse().ok())
            .collect()
    }

    /// #391: dropping the `run_windows_fallback` future mid-command must kill
    /// cmd.exe AND its children, not leave them running to completion.
    #[tokio::test]
    async fn dropping_the_future_kills_the_process_tree() {
        let cwd = std::env::temp_dir();
        let command = format!("ping -n {MARKER}");
        // Boxed (not `tokio::pin!`) so `drop(fut)` really drops the future.
        let mut fut =
            Box::pin(run_windows_fallback(
                &command,
                &cwd,
                Duration::from_secs(120),
                120_000,
                &cwd.join("claurst-tool-output-test"),
            ));

        // Let the command start, then confirm both cmd.exe and ping.exe are up.
        tokio::select! {
            res = &mut fut => panic!("command finished unexpectedly: {}", res.content),
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
        let before = marker_pids();
        assert!(
            before.len() >= 2,
            "expected cmd.exe and ping.exe to be running, found pids {before:?}"
        );

        // Cancel: drop the in-flight future.
        drop(fut);

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let lingering = marker_pids();
            if lingering.is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "process tree survived the dropped future; still running: {lingering:?}"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}
