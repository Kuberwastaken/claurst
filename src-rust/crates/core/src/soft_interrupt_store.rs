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
use std::io::Write;
use std::path::{Path, PathBuf};

/// Stable delivery identity is also the journaled Message UUID. Sequence and
/// priority reconstruct ordering; identity makes replay safe after a crash
/// between journal acceptance and removal from this store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedSoftInterrupt {
    pub id: String,
    pub content: String,
    pub seq: u64,
    pub priority: u8,
}

pub fn soft_interrupt_dir_in(config_dir: &Path) -> PathBuf {
    config_dir.join("pending-soft-interrupts")
}

fn path_for_session_in(config_dir: &Path, session_id: &str) -> crate::Result<PathBuf> {
    if session_id.is_empty()
        || session_id.contains('/')
        || session_id.contains('\\')
        || session_id.contains("..")
    {
        return Err(ClaudeError::Other(format!(
            "invalid session id for soft-interrupt store: {session_id}"
        )));
    }
    Ok(soft_interrupt_dir_in(config_dir).join(format!("{session_id}.json")))
}

fn read_in(config_dir: &Path, session_id: &str) -> crate::Result<Vec<PersistedSoftInterrupt>> {
    let path = path_for_session_in(config_dir, session_id)?;
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    serde_json::from_str(&raw).map_err(|error| {
        ClaudeError::Other(format!(
            "corrupt soft-interrupt store {}: {error}",
            path.display()
        ))
    })
}

fn store_lock(config_dir: &Path, session_id: &str) -> crate::Result<fslock::LockFile> {
    let path = path_for_session_in(config_dir, session_id)?;
    let directory = soft_interrupt_dir_in(config_dir);
    std::fs::create_dir_all(&directory)?;
    crate::accounts::set_user_only_dir_perms(&directory);
    let lock_path = path.with_extension("lock");
    let mut lock = fslock::LockFile::open(&lock_path)?;
    crate::accounts::set_user_only_perms(&lock_path);
    lock.lock_with_pid()?;
    Ok(lock)
}

// Caller holds the session's store lock for the entire read/modify/write.
fn write_in(
    config_dir: &Path,
    session_id: &str,
    interrupts: &[PersistedSoftInterrupt],
) -> crate::Result<()> {
    let path = path_for_session_in(config_dir, session_id)?;
    let directory = soft_interrupt_dir_in(config_dir);
    if interrupts.is_empty() {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    } else {
        let tmp = directory.join(format!(".{session_id}.{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> crate::Result<()> {
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&tmp)?;
            crate::accounts::set_user_only_perms(&tmp);
            file.write_all(&serde_json::to_vec_pretty(interrupts)?)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&tmp, &path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result?;
    }
    #[cfg(unix)]
    std::fs::File::open(&directory)?.sync_all()?;
    Ok(())
}

pub fn load_in(config_dir: &Path, session_id: &str) -> crate::Result<Vec<PersistedSoftInterrupt>> {
    read_in(config_dir, session_id)
}

/// Explicit destructive consumption; durable command delivery uses load plus
/// journal acceptance plus remove_by_ids instead.
pub fn take_in(config_dir: &Path, session_id: &str) -> crate::Result<Vec<PersistedSoftInterrupt>> {
    let _lock = store_lock(config_dir, session_id)?;
    let loaded = read_in(config_dir, session_id)?;
    if !loaded.is_empty() {
        write_in(config_dir, session_id, &[])?;
    }
    Ok(loaded)
}

pub fn overwrite_in(
    config_dir: &Path,
    session_id: &str,
    interrupts: &[PersistedSoftInterrupt],
) -> crate::Result<()> {
    let _lock = store_lock(config_dir, session_id)?;
    write_in(config_dir, session_id, interrupts)
}

/// Merge unacknowledged entries without removing another writer's input.
/// Retrying a previously persisted identity is idempotent.
pub fn merge_in(
    config_dir: &Path,
    session_id: &str,
    interrupts: &[PersistedSoftInterrupt],
) -> crate::Result<()> {
    let _lock = store_lock(config_dir, session_id)?;
    let mut current = read_in(config_dir, session_id)?;
    for interrupt in interrupts {
        match current.iter().find(|entry| entry.id == interrupt.id) {
            Some(existing) if existing != interrupt => {
                return Err(ClaudeError::Other(
                    "soft-interrupt identity reused with different content".into(),
                ))
            }
            Some(_) => {}
            None => current.push(interrupt.clone()),
        }
    }
    write_in(config_dir, session_id, &current)
}

pub fn append_in(
    config_dir: &Path,
    session_id: &str,
    interrupt: PersistedSoftInterrupt,
) -> crate::Result<()> {
    merge_in(config_dir, session_id, &[interrupt])
}

pub fn remove_by_ids_in(config_dir: &Path, session_id: &str, ids: &[String]) -> crate::Result<()> {
    let _lock = store_lock(config_dir, session_id)?;
    let mut current = read_in(config_dir, session_id)?;
    current.retain(|entry| !ids.contains(&entry.id));
    write_in(config_dir, session_id, &current)
}

pub fn remove_by_seqs_in(
    config_dir: &Path,
    session_id: &str,
    seqs: &[u64],
) -> crate::Result<Vec<u64>> {
    let _lock = store_lock(config_dir, session_id)?;
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
            id: format!("interrupt-{seq}"),
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
    fn concurrent_writers_and_identity_acknowledgements_do_not_drop_other_input() {
        let dir = tempfile::tempdir().unwrap();
        std::thread::scope(|scope| {
            for worker in 0..4 {
                let base = dir.path();
                scope.spawn(move || {
                    for index in 0..10 {
                        append_in(
                            base,
                            "session",
                            PersistedSoftInterrupt {
                                id: format!("{worker}-{index}"),
                                content: format!("input {worker}-{index}"),
                                seq: index,
                                priority: 1,
                            },
                        )
                        .unwrap();
                    }
                });
            }
        });
        assert_eq!(load_in(dir.path(), "session").unwrap().len(), 40);
        let ids: Vec<_> = (0..10).map(|index| format!("0-{index}")).collect();
        remove_by_ids_in(dir.path(), "session", &ids).unwrap();
        let remaining = load_in(dir.path(), "session").unwrap();
        assert_eq!(remaining.len(), 30);
        assert!(remaining.iter().all(|entry| !ids.contains(&entry.id)));
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
