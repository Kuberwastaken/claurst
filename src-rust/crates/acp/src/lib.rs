//! Agent Client Protocol (ACP) server for Claurst.
//!
//! ACP is the open protocol pioneered by Zed for standardizing communication
//! between AI coding agents and editors (Zed, Neovim, JetBrains, VS Code, …).
//! Spec: <https://agentclientprotocol.com>
//!
//! This crate turns the local `claurst` binary into a compliant ACP agent
//! over newline-delimited JSON-RPC 2.0 on stdio. Editors launch `claurst acp`
//! as a subprocess and drive it through the protocol's standard methods:
//!
//! | Method                       | Direction  | Notes                                       |
//! |------------------------------|------------|---------------------------------------------|
//! | `initialize`                 | C → A      | Capability negotiation                      |
//! | `authenticate`               | C → A      | No-op (Claurst uses local credentials)      |
//! | `session/new`                | C → A      | Create a session with cwd + MCP roster      |
//! | `session/prompt`             | C → A      | Run a turn; streams `session/update` events |
//! | `session/cancel`             | C → A (no resp) | Cancel an in-flight prompt             |
//! | `session/update`             | A → C (no resp) | Streamed text/tool deltas              |
//! | `session/request_permission` | A → C      | Tool approval dialog                        |
//!
//! Per-session MCP server configs supplied via `session/new` are accepted
//! but currently ignored in favour of the MCP servers configured in the
//! settings for the session's working directory. This will be resolved as part of the planned unified MCP routing work
//! (see `src-rust/plan/migration-todo.md`).

mod connection;
mod permission;
mod prompt;
mod runtime;
mod server;
mod sessions;

#[cfg(test)]
mod e2e_tests;

use tokio::sync::mpsc;
use tracing::{error, info, warn};

pub use connection::Connection;
pub use runtime::AgentRuntime;
pub use server::AgentServer;

/// Run the ACP server on the current process' stdin / stdout. Returns when
/// stdin reaches EOF.
pub async fn run_acp_server() -> anyhow::Result<()> {
    // We must NEVER write to stdout outside the protocol — every byte on
    // stdout is parsed by the client as JSON-RPC. Force logs to stderr.
    install_stderr_tracing();

    info!(version = env!("CARGO_PKG_VERSION"), "ACP: starting server");

    // Runtimes are built per session working directory on `session/new`.
    let connection = Connection::new(tokio::io::stdout());
    let server = AgentServer::new(connection.clone());

    let (tx, mut rx) = mpsc::unbounded_channel();
    // Keep the protocol channel private to this reader so child processes
    // (git, shells, MCP servers, ...) never inherit it. See `protocol_reader`.
    let reader_fut = connection::run_reader(connection, protocol_reader()?, tx);

    // Track in-flight dispatch tasks so they can finish writing their
    // responses before the runtime shuts down. The dispatch future only
    // returns once the reader has dropped `tx` (closing `rx`) AND every
    // spawned handler has resolved.
    let dispatch_fut = async {
        let mut tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();
        while let Some(msg) = rx.recv().await {
            tasks.push(server.dispatch(msg));
        }
        for handle in tasks {
            let _ = handle.await;
        }
    };

    let (reader_res, _) = tokio::join!(reader_fut, dispatch_fut);
    if let Err(e) = reader_res {
        error!(?e, "ACP: reader loop failed");
    }

    // Let turn-end memory extraction tasks finish before shutdown: ACP
    // clients disconnect right after the last turn response, and the
    // detached extraction spawn would die with the runtime.
    {
        let unfinished =
            claurst_query::await_pending_extractions(std::time::Duration::from_secs(30)).await;
        if unfinished > 0 {
            warn!(
                count = unfinished,
                "memory extraction tasks dropped at ACP shutdown"
            );
        }
    }

    info!("ACP: server shutdown");
    Ok(())
}

/// The reader side of the protocol, detached from what child processes inherit.
///
/// On Windows the ACP reader sits in a blocking `ReadFile` on the stdin pipe.
/// Pipe handles are synchronous, so any other I/O on the same handle blocks
/// until that read returns. A child that inherits our stdin (every `Command`
/// without an explicit `.stdin(..)`) queries it while starting up: `git status`
/// run for the system context during `session/new` never returned, so the
/// session was never created.
///
/// The reader therefore gets a private, non-inheritable duplicate of stdin,
/// and the process' standard input is pointed at NUL, which is what children
/// inherit from now on.
#[cfg(windows)]
fn protocol_reader() -> std::io::Result<tokio::fs::File> {
    use std::os::windows::io::{FromRawHandle, IntoRawHandle};

    type Handle = *mut core::ffi::c_void;
    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const DUPLICATE_SAME_ACCESS: u32 = 2;
    const HANDLE_FLAG_INHERIT: u32 = 1;
    extern "system" {
        fn GetStdHandle(n_std_handle: u32) -> Handle;
        fn SetStdHandle(n_std_handle: u32, handle: Handle) -> i32;
        fn GetCurrentProcess() -> Handle;
        fn SetHandleInformation(handle: Handle, mask: u32, flags: u32) -> i32;
        fn DuplicateHandle(
            source_process: Handle,
            source: Handle,
            target_process: Handle,
            target: *mut Handle,
            access: u32,
            inherit: i32,
            options: u32,
        ) -> i32;
    }

    // SAFETY: plain Win32 calls on this process' own handles. `dup` is owned by
    // the `File` created right after a successful `DuplicateHandle`, and the NUL
    // handle is intentionally leaked: it becomes the process' standard input.
    unsafe {
        let stdin = GetStdHandle(STD_INPUT_HANDLE);
        if stdin.is_null() || stdin as isize == -1 {
            return Err(std::io::Error::last_os_error());
        }

        let process = GetCurrentProcess();
        let mut dup: Handle = core::ptr::null_mut();
        if DuplicateHandle(
            process,
            stdin,
            process,
            &mut dup,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        ) == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        let private = std::fs::File::from_raw_handle(dup);

        let nul = std::fs::OpenOptions::new()
            .read(true)
            .open("NUL")?
            .into_raw_handle();
        if SetStdHandle(STD_INPUT_HANDLE, nul) == 0 {
            let err = std::io::Error::last_os_error();
            drop(std::fs::File::from_raw_handle(nul));
            return Err(err);
        }

        // Best effort: the original handle should not travel to children either.
        SetHandleInformation(stdin, HANDLE_FLAG_INHERIT, 0);
        Ok(tokio::fs::File::from_std(private))
    }
}

#[cfg(not(windows))]
fn protocol_reader() -> std::io::Result<tokio::io::Stdin> {
    Ok(tokio::io::stdin())
}

#[cfg(all(test, windows))]
mod stdin_tests {
    use super::protocol_reader;
    use std::io::Write;
    use std::os::windows::io::FromRawHandle;
    use tokio::io::AsyncReadExt;

    type Handle = *mut core::ffi::c_void;
    const STD_INPUT_HANDLE: u32 = -10i32 as u32;
    const FILE_TYPE_CHAR: u32 = 2;
    const FILE_TYPE_PIPE: u32 = 3;
    extern "system" {
        fn GetStdHandle(n_std_handle: u32) -> Handle;
        fn SetStdHandle(n_std_handle: u32, handle: Handle) -> i32;
        fn GetFileType(handle: Handle) -> u32;
        fn CreatePipe(
            read: *mut Handle,
            write: *mut Handle,
            attrs: *mut core::ffi::c_void,
            size: u32,
        ) -> i32;
    }

    /// Children must inherit NUL, not the protocol pipe, and the reader must
    /// still receive what the client writes.
    #[tokio::test]
    async fn children_do_not_inherit_the_protocol_pipe() {
        // SAFETY: test-only Win32 calls on handles created here; the original
        // standard input is restored before the test ends.
        unsafe {
            let original = GetStdHandle(STD_INPUT_HANDLE);
            let (mut read, mut write): (Handle, Handle) =
                (core::ptr::null_mut(), core::ptr::null_mut());
            assert_ne!(
                CreatePipe(&mut read, &mut write, core::ptr::null_mut(), 0),
                0
            );
            assert_ne!(SetStdHandle(STD_INPUT_HANDLE, read), 0);
            assert_eq!(GetFileType(GetStdHandle(STD_INPUT_HANDLE)), FILE_TYPE_PIPE);

            let mut reader = protocol_reader().expect("protocol reader");
            assert_eq!(GetFileType(GetStdHandle(STD_INPUT_HANDLE)), FILE_TYPE_CHAR);

            let mut client = std::fs::File::from_raw_handle(write);
            client
                .write_all(b"hello\n")
                .expect("write to protocol pipe");
            let mut buf = [0u8; 6];
            reader
                .read_exact(&mut buf)
                .await
                .expect("read from protocol pipe");
            assert_eq!(&buf, b"hello\n");

            SetStdHandle(STD_INPUT_HANDLE, original);
        }
    }
}

fn install_stderr_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let _ = fmt()
        .with_env_filter(
            EnvFilter::try_from_env("CLAURST_ACP_LOG").unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();
}
