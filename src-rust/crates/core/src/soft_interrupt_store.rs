// soft_interrupt_store.rs — durable queue for mid-turn steering input.
//
// The TUI pushes user messages typed while a turn is running (steers) and
// background-task completion notices into the shared `CommandQueue`, which the
// query loop drains before its next model request (#444). That queue lives in
// memory: a crash or restart while a turn runs loses whatever the user typed
// into it. This store mirrors that queue under
// `<config_dir>/pending-soft-interrupts/<session_id>.json` so undelivered
// input survives the process and is replayed into the next session run.
//
// Ported from jcode's `soft_interrupt_store.rs`; persistence only. The
// injection points themselves are claurst's step-boundary drains.

use crate::ClaudeError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A queued steering message as stored on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedSoftInterrupt {
    pub content: String,
    /// Push order within one drain, oldest first.
    pub seq: u64,
    /// Push priority: higher drains first. Kept so the heap order of the
    /// in-memory queue can be reconstructed exactly.
    pub priority: u8,
}

/// Directory holding one JSON file per session with undelivered input.
pub fn soft_interrupt_dir_in(config_dir: &Path) -> PathBuf {
    config_dir.join("pending-soft-interrupts")
}

fn path_for_session_in(config_dir: &Path, session_id: &str) -> crate::Result<PathBuf> {
    // Same traversal guard as `transcript_path` (issue #204).
    if session_id.contains('/') || session_id.contains('\\') || session_id.contains("..") {
        return Err(ClaudeError::Other(format!(
            "invalid session id for soft-interrupt store: {session_id}"
        )));
    }
    Ok(soft_interrupt_dir_in(config_dir).join(format!("{session_id}.json")))
}

fn read_in(config_dir: &Path, session_id: &str) -> crate::Result<Vec<PersistedSoftInterrupt>> {
    let path = path_for_session_in(config_dir, session_id)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path)?;
    serde_json::from_str(&raw).map_err(|err| {
        ClaudeError::Other(format!(
            "corrupt soft-interrupt store {}: {err}",
            path.display()
        ))
    })
}

fn write_in(
    config_dir: &Path,
    session_id: &str,
    interrupts: &[PersistedSoftInterrupt],
) -> crate::Result<()> {
    let path = path_for_session_in(config_dir, session_id)?;
    if interrupts.is_empty() {
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write-then-rename keeps readers from ever seeing a torn file.
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(interrupts)?;
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Load the undelivered interrupts persisted for `session_id`.
pub fn load_in(config_dir: &Path, session_id: &str) -> crate::Result<Vec<PersistedSoftInterrupt>> {
    read_in(config_dir, session_id)
}

/// Load and delete the persisted interrupts for `session_id`.
pub fn take_in(config_dir: &Path, session_id: &str) -> crate::Result<Vec<PersistedSoftInterrupt>> {
    let loaded = read_in(config_dir, session_id)?;
    if !loaded.is_empty() {
        write_in(config_dir, session_id, &[])?;
    }
    Ok(loaded)
}

/// Replace the persisted interrupts for `session_id`.
pub fn overwrite_in(
    config_dir: &Path,
    session_id: &str,
    interrupts: &[PersistedSoftInterrupt],
) -> crate::Result<()> {
    write_in(config_dir, session_id, interrupts)
}

/// Append one interrupt to those persisted for `session_id`.
pub fn append_in(
    config_dir: &Path,
    session_id: &str,
    interrupt: PersistedSoftInterrupt,
) -> crate::Result<()> {
    let mut current = read_in(config_dir, session_id)?;
    current.push(interrupt);
    write_in(config_dir, session_id, &current)
}

/// Remove the persisted interrupts whose `seq` is in `seqs`, returning the
/// removed `seq`s in store order. Mirrors `CommandQueue::remove` recovery.
pub fn remove_by_seqs_in(
    config_dir: &Path,
    session_id: &str,
    seqs: &[u64],
) -> crate::Result<Vec<u64>> {
    let mut current = read_in(config_dir, session_id)?;
    let mut removed = Vec::new();
    current.retain(|interrupt| {
        if seqs.contains(&interrupt.seq) {
            removed.push(interrupt.seq);
            false
        } else {
            true
        }
    });
    if !removed.is_empty() {
        write_in(config_dir, session_id, &current)?;
    }
    Ok(removed)
}

// Default-config-dir wrappers over the `_in` variants above.

fn config_dir() -> PathBuf {
    crate::config::Settings::config_dir()
}

pub fn load(session_id: &str) -> crate::Result<Vec<PersistedSoftInterrupt>> {
    load_in(&config_dir(), session_id)
}

pub fn take(session_id: &str) -> crate::Result<Vec<PersistedSoftInterrupt>> {
    take_in(&config_dir(), session_id)
}

pub fn overwrite(session_id: &str, interrupts: &[PersistedSoftInterrupt]) -> crate::Result<()> {
    overwrite_in(&config_dir(), session_id, interrupts)
}

pub fn append(session_id: &str, interrupt: PersistedSoftInterrupt) -> crate::Result<()> {
    append_in(&config_dir(), session_id, interrupt)
}

pub fn remove_by_seqs(session_id: &str, seqs: &[u64]) -> crate::Result<Vec<u64>> {
    remove_by_seqs_in(&config_dir(), session_id, seqs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interrupt(content: &str, seq: u64, priority: u8) -> PersistedSoftInterrupt {
        PersistedSoftInterrupt {
            content: content.to_string(),
            seq,
            priority,
        }
    }

    #[test]
    fn append_take_and_overwrite_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let session = "ses_soft_interrupt_store";

        append_in(base, session, interrupt("hello", 0, 1)).unwrap();
        append_in(base, session, interrupt("world", 1, 3)).unwrap();

        let loaded = load_in(base, session).unwrap();
        assert_eq!(
            loaded,
            vec![interrupt("hello", 0, 1), interrupt("world", 1, 3)]
        );

        let taken = take_in(base, session).unwrap();
        assert_eq!(taken.len(), 2);
        assert!(load_in(base, session).unwrap().is_empty());
        assert!(
            !soft_interrupt_dir_in(base)
                .join(format!("{session}.json"))
                .exists(),
            "take removes the file"
        );

        // Overwrite with empty clears too.
        append_in(base, session, interrupt("later", 0, 1)).unwrap();
        overwrite_in(base, session, &[]).unwrap();
        assert!(load_in(base, session).unwrap().is_empty());
    }

    #[test]
    fn take_on_missing_session_is_empty_without_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(take_in(dir.path(), "never-seen").unwrap().is_empty());
    }

    #[test]
    fn remove_by_seqs_drops_only_the_requested_entries() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let session = "ses_remove_by_seqs";
        for (seq, priority) in [(0, 1), (1, 1), (2, 1)] {
            append_in(base, session, interrupt("x", seq, priority)).unwrap();
        }
        assert_eq!(remove_by_seqs_in(base, session, &[1]).unwrap(), vec![1]);
        assert_eq!(
            load_in(base, session).unwrap(),
            vec![interrupt("x", 0, 1), interrupt("x", 2, 1)]
        );
        // Removing an absent seq is a no-op and leaves the file alone.
        assert!(remove_by_seqs_in(base, session, &[9]).unwrap().is_empty());
        assert!(remove_by_seqs_in(base, session, &[]).unwrap().is_empty());
        assert!(
            soft_interrupt_dir_in(base)
                .join(format!("{session}.json"))
                .exists(),
            "no-op removals keep the file"
        );
    }

    #[test]
    fn sessions_are_isolated_per_file() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        append_in(base, "ses-a", interrupt("a", 0, 1)).unwrap();
        append_in(base, "ses-b", interrupt("b", 0, 1)).unwrap();
        assert_eq!(take_in(base, "ses-a").unwrap().len(), 1);
        assert_eq!(take_in(base, "ses-b").unwrap().len(), 1);
    }

    #[test]
    fn traversal_session_id_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let err = load_in(dir.path(), "../../etc").unwrap_err();
        assert!(matches!(err, ClaudeError::Other(_)));
    }

    #[test]
    fn corrupt_store_file_surfaces_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path();
        let store_dir = soft_interrupt_dir_in(base);
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::write(store_dir.join("ses-corrupt.json"), "not json").unwrap();
        assert!(load_in(base, "ses-corrupt").is_err());
    }
}
