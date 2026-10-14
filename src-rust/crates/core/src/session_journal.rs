//! Crash-safe persistence for `~/.claurst/sessions` (issue #449).
//!
//! Each session id owns three files in the sessions directory:
//!
//! - `<id>.json`, the snapshot: the whole [`ConversationSession`], rewritten
//!   atomically (temp file in the same directory, fsync, rename) at turn end
//!   and on idle metadata changes. Its `journal_seq` names the last journal
//!   entry it includes.
//! - `<id>.jsonl`, the journal: append-only, one `{"seq":N,"entry":{..}}` per
//!   line. The query loop appends after every assistant message and every
//!   tool-result batch, so a crash loses at most the step in flight. It is
//!   never rewritten, so it also holds the messages a later rewind or
//!   compaction replaced.
//! - `<id>.lock`: an advisory lock (`flock` / `LockFileEx`) held by the one
//!   process writing the session. The OS drops it when that process exits,
//!   so a crash never leaves a stale lock. Another process can still read
//!   the session; its writes fail with [`SessionLocked`].
//!
//! Loading parses the snapshot and replays the journal entries numbered above
//! its `journal_seq`. When the snapshot is missing or does not parse (a crash
//! before the first checkpoint, or a torn in-place write by an older build),
//! the session is rebuilt from the journal alone, which always starts with a
//! `meta` entry followed by the full history. Journal lines that do not parse
//! are skipped, and the writer cuts a torn last line before appending.
//! Snapshots written before the journal existed have `journal_seq` 0 and no
//! journal; they load as before, and their journal is started (with the full
//! history) the first time they are written.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::hash::Hasher;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::history::{CompactionRecord, ConversationSession};
use crate::types::Message;

/// Another process holds the session's lock, so this one must not write it.
#[derive(Debug, thiserror::Error)]
#[error("session {0} is open in another claurst process; changes made here are not saved")]
pub struct SessionLocked(pub String);

fn snapshot_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.json"))
}

fn journal_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.jsonl"))
}

fn lock_path(dir: &Path, id: &str) -> PathBuf {
    dir.join(format!("{id}.lock"))
}

/// Everything in a session except its history, checkpoints and compactions.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionMeta {
    id: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    model: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    working_dir: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    branch_from: Option<String>,
    #[serde(default)]
    branch_at_message: Option<usize>,
    #[serde(default)]
    remote_session_url: Option<String>,
    #[serde(default)]
    total_cost: f64,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    parent_session_id: Option<String>,
    #[serde(default)]
    fork_point_message_index: Option<usize>,
}

impl SessionMeta {
    fn of(session: &ConversationSession) -> Self {
        Self {
            id: session.id.clone(),
            created_at: session.created_at,
            updated_at: session.updated_at,
            model: session.model.clone(),
            title: session.title.clone(),
            working_dir: session.working_dir.clone(),
            tags: session.tags.clone(),
            branch_from: session.branch_from.clone(),
            branch_at_message: session.branch_at_message,
            remote_session_url: session.remote_session_url.clone(),
            total_cost: session.total_cost,
            total_tokens: session.total_tokens,
            parent_session_id: session.parent_session_id.clone(),
            fork_point_message_index: session.fork_point_message_index,
        }
    }

    fn apply(self, session: &mut ConversationSession) {
        session.id = self.id;
        session.created_at = self.created_at;
        session.updated_at = self.updated_at;
        session.model = self.model;
        session.title = self.title;
        session.working_dir = self.working_dir;
        session.tags = self.tags;
        session.branch_from = self.branch_from;
        session.branch_at_message = self.branch_at_message;
        session.remote_session_url = self.remote_session_url;
        session.total_cost = self.total_cost;
        session.total_tokens = self.total_tokens;
        session.parent_session_id = self.parent_session_id;
        session.fork_point_message_index = self.fork_point_message_index;
    }
}

/// One journal entry. Replaying the entries in order rebuilds the history.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Entry<'a> {
    /// Session metadata. The first entry of every journal; repeated at each
    /// checkpoint and metadata change.
    Meta { session: SessionMeta },
    /// Append one message.
    Message { message: Cow<'a, Message> },
    /// Drop the messages from `len` on (a rewind or another history rewrite).
    Truncate { len: usize },
    /// A compaction: the head `[0, record.replaced_len())` moves into
    /// `record.replaced` and `summary` takes its place. The record is stored
    /// without `replaced`; the preceding entries already hold those messages.
    Compaction {
        record: CompactionRecord,
        summary: Cow<'a, Message>,
    },
    /// Drop the compactions from `keep` on (a rewind past a compaction).
    DropCompactions { keep: usize },
}

#[derive(Serialize, Deserialize)]
struct JournalLine<'a> {
    seq: u64,
    entry: Entry<'a>,
}

fn apply(session: &mut ConversationSession, entry: Entry<'_>) {
    match entry {
        Entry::Meta { session: meta } => meta.apply(session),
        Entry::Message { message } => session.messages.push(message.into_owned()),
        Entry::Truncate { len } => session.messages.truncate(len),
        Entry::Compaction { mut record, summary } => {
            let mut head = std::mem::take(&mut session.messages);
            let tail = head.split_off(record.replaced_len().min(head.len()));
            session.messages.reserve(tail.len() + 1);
            session.messages.push(summary.into_owned());
            session.messages.extend(tail);
            record.replaced = head;
            session.compactions.push(record);
        }
        Entry::DropCompactions { keep } => session.compactions.truncate(keep),
    }
}

/// The parseable entries of a journal and the byte length of its complete
/// lines (anything after that is a torn tail).
struct JournalScan {
    entries: Vec<(u64, Entry<'static>)>,
    valid_len: u64,
}

fn scan_journal(path: &Path) -> std::io::Result<JournalScan> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(JournalScan { entries: Vec::new(), valid_len: 0 })
        }
        Err(e) => return Err(e),
    };
    let mut entries = Vec::new();
    let mut start = 0;
    while let Some(len) = bytes[start..].iter().position(|&b| b == b'\n') {
        let line = &bytes[start..start + len];
        match serde_json::from_slice::<JournalLine<'static>>(line) {
            Ok(parsed) => entries.push((parsed.seq, parsed.entry)),
            Err(e) if !line.iter().all(u8::is_ascii_whitespace) => warn!(
                path = %path.display(),
                offset = start,
                error = %e,
                "Skipping an unreadable session journal line"
            ),
            Err(_) => {}
        }
        start += len + 1;
    }
    if start < bytes.len() {
        warn!(
            path = %path.display(),
            bytes = bytes.len() - start,
            "Ignoring the torn last line of a session journal"
        );
    }
    Ok(JournalScan { entries, valid_len: start as u64 })
}

/// `None` when the snapshot does not exist or does not parse.
fn read_snapshot(path: &Path) -> std::io::Result<Option<ConversationSession>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    match serde_json::from_slice(&bytes) {
        Ok(session) => Ok(Some(session)),
        Err(e) => {
            warn!(
                path = %path.display(),
                error = %e,
                "Session snapshot is unreadable (torn write?); falling back to its journal"
            );
            Ok(None)
        }
    }
}

/// A session read from disk, with how it was recovered.
pub struct LoadedSession {
    pub session: ConversationSession,
    /// Journal entries applied on top of the snapshot (or, when `rebuilt`,
    /// the whole journal).
    pub replayed: usize,
    /// The snapshot was missing or unreadable and the journal alone was used.
    pub rebuilt: bool,
}

/// Load session `id` from `dir`: the snapshot plus the journal entries newer
/// than it, or the journal alone when the snapshot is missing or torn.
pub fn load(dir: &Path, id: &str) -> anyhow::Result<LoadedSession> {
    load_with(dir, id, false)
}

/// `skip_older_journal`: trust the snapshot when the journal was last
/// modified before it (listing only; [`load`] always reads the journal).
fn load_with(dir: &Path, id: &str, skip_older_journal: bool) -> anyhow::Result<LoadedSession> {
    let snapshot_file = snapshot_path(dir, id);
    let journal_file = journal_path(dir, id);
    if let Some(mut session) = read_snapshot(&snapshot_file)? {
        let journal_is_older = skip_older_journal
            && matches!(
                (
                    std::fs::metadata(&snapshot_file).and_then(|m| m.modified()),
                    std::fs::metadata(&journal_file).and_then(|m| m.modified()),
                ),
                (Ok(snapshot_time), Ok(journal_time)) if journal_time < snapshot_time
            );
        if journal_is_older {
            return Ok(LoadedSession { session, replayed: 0, rebuilt: false });
        }
        let base = session.journal_seq;
        let mut replayed = 0;
        for (seq, entry) in scan_journal(&journal_file)?.entries {
            if seq > base {
                apply(&mut session, entry);
                session.journal_seq = seq;
                replayed += 1;
            }
        }
        if replayed > 0 {
            info!(
                session = id,
                replayed,
                "Session journal is newer than the snapshot; replayed the newer entries"
            );
        }
        return Ok(LoadedSession { session, replayed, rebuilt: false });
    }

    if !snapshot_file.exists() && !journal_file.exists() {
        // An `io::Error` so callers can tell "no such session" apart.
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("session {id} not found"),
        )
        .into());
    }
    let mut entries = scan_journal(&journal_file)?.entries.into_iter();
    let Some((seq, Entry::Meta { session: meta })) = entries.next() else {
        anyhow::bail!("session {id} has no readable snapshot or journal");
    };
    let mut session = ConversationSession::new(String::new());
    meta.apply(&mut session);
    session.journal_seq = seq;
    let mut replayed = 1;
    for (seq, entry) in entries {
        apply(&mut session, entry);
        session.journal_seq = seq;
        replayed += 1;
    }
    warn!(
        session = id,
        replayed,
        "Session snapshot missing or unreadable; rebuilt it from the journal"
    );
    Ok(LoadedSession { session, replayed, rebuilt: true })
}

/// Every session in `dir` that loads, including ones that only have a journal
/// (a crash before their first snapshot).
pub fn list(dir: &Path) -> Vec<ConversationSession> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let ids: BTreeSet<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            match path.extension().and_then(|e| e.to_str()) {
                Some("json" | "jsonl") => {
                    path.file_stem().and_then(|s| s.to_str()).map(str::to_owned)
                }
                _ => None,
            }
        })
        .collect();
    ids.iter()
        .filter_map(|id| load_with(dir, id, true).ok())
        .map(|loaded| loaded.session)
        .collect()
}

/// Delete a session's snapshot, journal and lock file. Fails with
/// [`SessionLocked`] while any writer (this process included) holds it.
pub fn delete(dir: &Path, id: &str) -> anyhow::Result<()> {
    let lock_file = lock_path(dir, id);
    if lock_file.exists() {
        let mut lock = fslock::LockFile::open(&lock_file)?;
        if !lock.try_lock()? {
            return Err(SessionLocked(id.to_owned()).into());
        }
        // Released before the files go: Windows cannot remove a file this
        // process still has open.
        drop(lock);
    }
    for path in [snapshot_path(dir, id), journal_path(dir, id), lock_file] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Checkpoint `session` into `dir` through this process's live writer for it,
/// or a short-lived one that takes and releases the lock.
pub fn save(dir: &Path, session: &mut ConversationSession) -> anyhow::Result<()> {
    let key = snapshot_path(dir, &session.id);
    let live = LIVE.lock().get(&key).and_then(Weak::upgrade);
    let journal = live.unwrap_or_else(|| SessionJournal::new(dir, session));
    journal.checkpoint(session)
}

/// The writers alive in this process, by snapshot path, so a standalone
/// [`save`] of a session this process is writing goes through its writer
/// instead of failing on its own lock.
static LIVE: Lazy<Mutex<HashMap<PathBuf, Weak<SessionJournal>>>> = Lazy::new(Default::default);

/// Hash of a message's serialized form, used to find what the journal
/// already holds without keeping a second copy of the history.
fn message_hash(message: &Message) -> u64 {
    struct HashWriter(std::collections::hash_map::DefaultHasher);
    impl Write for HashWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Hasher::write(&mut self.0, buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(Default::default());
    // Cannot fail: the writer never errors and a Message has only string keys.
    let _ = serde_json::to_writer(&mut writer, message);
    writer.0.finish()
}

/// Entries that turn the history `persisted` describes into `messages`: a
/// truncate to their common prefix (when they diverge) and the new messages.
fn history_diff<'m>(persisted: &[u64], messages: &[&'m Message]) -> (Vec<Entry<'m>>, Vec<u64>) {
    let hashes: Vec<u64> = messages.iter().map(|m| message_hash(m)).collect();
    let common = persisted
        .iter()
        .zip(&hashes)
        .take_while(|(a, b)| a == b)
        .count();
    let mut entries = Vec::new();
    if common < persisted.len() {
        entries.push(Entry::Truncate { len: common });
    }
    entries.extend(
        messages[common..]
            .iter()
            .map(|m| Entry::Message { message: Cow::Borrowed(*m) }),
    );
    (entries, hashes)
}

/// The writer for one session: owns its lock and appends to its journal.
/// Nothing is created and no lock is taken until the first write.
pub struct SessionJournal {
    dir: PathBuf,
    id: String,
    /// Journal sequence the session was loaded at: new entries are numbered
    /// above it even when the journal file itself was lost.
    base_seq: u64,
    /// Written as the first entry of a new journal.
    header: SessionMeta,
    open: Mutex<Option<OpenJournal>>,
}

struct OpenJournal {
    _lock: fslock::LockFile,
    file: File,
    seq: u64,
    /// The file has no entries yet, so the next append starts with the
    /// header: `meta` then `truncate {len: 0}`. The truncate makes the
    /// journal self-describing: replayed onto an older snapshot (one written
    /// before the journal existed, or after the journal file was lost) it
    /// replaces that snapshot's history instead of appending a second copy.
    needs_header: bool,
    /// [`message_hash`] of each message in the journal's history.
    persisted: Vec<u64>,
    /// Number of compactions in the journal's history.
    compactions: usize,
}

impl SessionJournal {
    /// A writer for `session` in `dir`.
    pub fn new(dir: &Path, session: &ConversationSession) -> Arc<Self> {
        let journal = Arc::new(Self {
            dir: dir.to_path_buf(),
            id: session.id.clone(),
            base_seq: session.journal_seq,
            header: SessionMeta::of(session),
            open: Mutex::new(None),
        });
        LIVE.lock()
            .insert(snapshot_path(dir, &session.id), Arc::downgrade(&journal));
        journal
    }

    /// A writer for `session` in the default sessions directory.
    pub fn for_session(session: &ConversationSession) -> Arc<Self> {
        Self::new(&crate::history::sessions_dir(), session)
    }

    /// Take the lock now, so a session held by another process is reported
    /// when it is opened rather than at its first write.
    pub fn acquire(&self) -> anyhow::Result<()> {
        self.with_open(|_, _| Ok(()))
    }

    /// Journal the difference between the persisted history and `messages`.
    /// Cheap when nothing changed (no write).
    pub fn sync_messages(&self, messages: &[Message]) -> anyhow::Result<()> {
        let messages: Vec<&Message> = messages.iter().collect();
        self.with_open(|journal, open| {
            let (entries, hashes) = history_diff(&open.persisted, &messages);
            if !entries.is_empty() {
                journal.append(open, entries)?;
            }
            open.persisted = hashes;
            Ok(())
        })
    }

    /// Journal a compaction: `record` (with `replaced` filled in by
    /// [`crate::history::apply_compaction`]) and the compacted history.
    pub fn record_compaction(
        &self,
        record: &CompactionRecord,
        compacted: &[Message],
    ) -> anyhow::Result<()> {
        let Some((summary, tail)) = compacted.split_first() else {
            return self.sync_messages(compacted);
        };
        let previous: Vec<&Message> = record.replaced.iter().chain(tail).collect();
        self.with_open(|journal, open| {
            let (mut entries, hashes) = history_diff(&open.persisted, &previous);
            entries.push(Entry::Compaction {
                record: record.without_replaced(),
                summary: Cow::Borrowed(summary),
            });
            journal.append(open, entries)?;
            open.persisted = std::iter::once(message_hash(summary))
                .chain(hashes.into_iter().skip(record.replaced.len()))
                .collect();
            open.compactions += 1;
            Ok(())
        })
    }

    /// Journal the metadata only. Used while a query is running: the loop
    /// owns the history until it finishes.
    pub fn record_meta(&self, session: &ConversationSession) -> anyhow::Result<()> {
        self.with_open(|journal, open| {
            journal.append(open, vec![Entry::Meta { session: SessionMeta::of(session) }])
        })
    }

    /// Journal whatever `session` changed, then rewrite its snapshot
    /// atomically with `journal_seq` set to the last entry.
    pub fn checkpoint(&self, session: &mut ConversationSession) -> anyhow::Result<()> {
        let seq = {
            let messages: Vec<&Message> = session.messages.iter().collect();
            let compactions = session.compactions.len();
            let meta = SessionMeta::of(session);
            self.with_open(|journal, open| {
                let (mut entries, hashes) = history_diff(&open.persisted, &messages);
                if compactions < open.compactions {
                    entries.push(Entry::DropCompactions { keep: compactions });
                    open.compactions = compactions;
                }
                entries.push(Entry::Meta { session: meta });
                journal.append(open, entries)?;
                open.persisted = hashes;
                Ok(open.seq)
            })?
        };
        session.journal_seq = seq;
        self.write_snapshot(session)
    }

    /// Run `f` on the open journal, opening it (and taking the lock) first.
    /// A failed write closes it so the next one re-reads the file and cuts
    /// any partial line before appending.
    fn with_open<T>(
        &self,
        f: impl FnOnce(&Self, &mut OpenJournal) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut slot = self.open.lock();
        let open = match slot.take() {
            Some(open) => open,
            None => self.open_journal()?,
        };
        let open = slot.insert(open);
        let result = f(self, open);
        if result.is_err() {
            *slot = None;
        }
        result
    }

    fn open_journal(&self) -> anyhow::Result<OpenJournal> {
        std::fs::create_dir_all(&self.dir)?;
        crate::accounts::set_user_only_dir_perms(&self.dir);
        let lock_file = lock_path(&self.dir, &self.id);
        let mut lock = fslock::LockFile::open(&lock_file)?;
        crate::accounts::set_user_only_perms(&lock_file);
        if !lock.try_lock_with_pid()? {
            return Err(SessionLocked(self.id.clone()).into());
        }
        let path = journal_path(&self.dir, &self.id);
        let scan = scan_journal(&path)?;
        let needs_header = scan.entries.is_empty();
        let mut history = ConversationSession::new(String::new());
        let mut seq = self.base_seq;
        for (entry_seq, entry) in scan.entries {
            apply(&mut history, entry);
            seq = seq.max(entry_seq);
        }
        let on_disk = match std::fs::metadata(&path) {
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(e.into()),
        };
        if on_disk > scan.valid_len {
            warn!(path = %path.display(), "Cutting the torn last line of the session journal");
            // A separate write handle: on Windows an append-mode handle has
            // no FILE_WRITE_DATA, so `set_len` on it is denied.
            OpenOptions::new().write(true).open(&path)?.set_len(scan.valid_len)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        // Session transcripts can contain secrets pulled into context; keep
        // them owner-only (issue #212).
        crate::accounts::set_user_only_perms(&path);
        Ok(OpenJournal {
            _lock: lock,
            file,
            seq,
            needs_header,
            persisted: history.messages.iter().map(message_hash).collect(),
            compactions: history.compactions.len(),
        })
    }

    /// Append `entries` (after the header, for a new journal) as one write
    /// and flush them to disk.
    fn append(&self, open: &mut OpenJournal, entries: Vec<Entry<'_>>) -> anyhow::Result<()> {
        let header = if open.needs_header {
            vec![
                Entry::Meta { session: self.header.clone() },
                Entry::Truncate { len: 0 },
            ]
        } else {
            Vec::new()
        };
        let mut seq = open.seq;
        let mut buf = Vec::new();
        for entry in header.into_iter().chain(entries) {
            seq += 1;
            serde_json::to_writer(&mut buf, &JournalLine { seq, entry })?;
            buf.push(b'\n');
        }
        open.file.write_all(&buf)?;
        open.file.sync_data()?;
        open.seq = seq;
        open.needs_header = false;
        Ok(())
    }

    /// Write the snapshot to a temp file in the same directory, flush it and
    /// rename it over `<id>.json`, so a crash leaves the old or the new file,
    /// never a torn one.
    fn write_snapshot(&self, session: &ConversationSession) -> anyhow::Result<()> {
        let tmp = self.dir.join(format!(".{}.json.tmp", self.id));
        let bytes = serde_json::to_vec_pretty(session)?;
        let mut file = File::create(&tmp)?;
        crate::accounts::set_user_only_perms(&tmp);
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, snapshot_path(&self.dir, &self.id))?;
        Ok(())
    }
}

impl Drop for SessionJournal {
    fn drop(&mut self) {
        LIVE.lock().retain(|_, journal| journal.strong_count() > 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{apply_compaction, CompactionTrigger};

    fn texts(messages: &[Message]) -> Vec<String> {
        messages.iter().map(|m| m.get_all_text()).collect()
    }

    fn session_with(texts: &[&str]) -> ConversationSession {
        let mut session = ConversationSession::new("test-model".to_string());
        session.title = Some("journal test".to_string());
        for (i, text) in texts.iter().enumerate() {
            session.add_message(if i % 2 == 0 {
                Message::user(*text)
            } else {
                Message::assistant(*text)
            });
        }
        session
    }

    /// Compact `session` the way the query loop does: the head before
    /// `split` is replaced by a summary.
    fn compact(session: &mut ConversationSession, split: usize, summary: &str) -> CompactionRecord {
        let mut compacted = vec![Message::user(summary)];
        compacted.extend_from_slice(&session.messages[split..]);
        let mut record = CompactionRecord {
            at: Utc::now(),
            trigger: CompactionTrigger::Manual,
            summary_uuid: String::new(),
            messages_before: session.messages.len(),
            messages_after: compacted.len(),
            tokens_before: 0,
            tokens_after: 0,
            summarised: true,
            replaced: Vec::new(),
        };
        apply_compaction(&mut session.messages, compacted, &mut record);
        record
    }

    fn journal_lines(dir: &Path, id: &str) -> Vec<String> {
        std::fs::read_to_string(journal_path(dir, id))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn crash_between_steps_recovers_from_the_journal_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with(&["u1"]);
        let journal = SessionJournal::new(dir.path(), &session);
        journal.sync_messages(&session.messages).unwrap();
        session.add_message(Message::assistant("a1"));
        journal.sync_messages(&session.messages).unwrap();
        // Crash: no checkpoint, so there is no snapshot.
        drop(journal);
        assert!(!snapshot_path(dir.path(), &session.id).exists());

        let loaded = load(dir.path(), &session.id).unwrap();
        assert!(loaded.rebuilt);
        assert_eq!(texts(&loaded.session.messages), ["u1", "a1"]);
        assert_eq!(loaded.session.title.as_deref(), Some("journal test"));
        assert_eq!(loaded.session.id, session.id);
        // Listed even though it never reached a snapshot.
        assert_eq!(list(dir.path()).len(), 1);
    }

    #[test]
    fn journal_newer_than_the_snapshot_is_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with(&["u1", "a1"]);
        let journal = SessionJournal::new(dir.path(), &session);
        journal.checkpoint(&mut session).unwrap();
        session.add_message(Message::user("u2"));
        session.add_message(Message::assistant("a2"));
        journal.sync_messages(&session.messages).unwrap();
        drop(journal);

        let loaded = load(dir.path(), &session.id).unwrap();
        assert!(!loaded.rebuilt);
        assert_eq!(loaded.replayed, 2);
        assert_eq!(texts(&loaded.session.messages), ["u1", "a1", "u2", "a2"]);
    }

    #[test]
    fn torn_snapshot_is_rebuilt_from_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with(&["u1", "a1", "u2"]);
        save(dir.path(), &mut session).unwrap();
        let path = snapshot_path(dir.path(), &session.id);
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap();

        let loaded = load(dir.path(), &session.id).unwrap();
        assert!(loaded.rebuilt);
        assert_eq!(texts(&loaded.session.messages), ["u1", "a1", "u2"]);
    }

    #[test]
    fn torn_journal_tail_is_skipped_and_cut_before_the_next_append() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with(&["u1", "a1"]);
        let journal = SessionJournal::new(dir.path(), &session);
        journal.sync_messages(&session.messages).unwrap();
        drop(journal);
        let path = journal_path(dir.path(), &session.id);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(br#"{"seq":99,"entry":{"type":"mess"#).unwrap();
        drop(file);

        let loaded = load(dir.path(), &session.id).unwrap();
        assert_eq!(texts(&loaded.session.messages), ["u1", "a1"]);

        session.add_message(Message::user("u2"));
        let journal = SessionJournal::new(dir.path(), &loaded.session);
        journal.sync_messages(&session.messages).unwrap();
        drop(journal);
        for line in journal_lines(dir.path(), &session.id) {
            serde_json::from_str::<JournalLine<'static>>(&line).unwrap();
        }
        let loaded = load(dir.path(), &session.id).unwrap();
        assert_eq!(texts(&loaded.session.messages), ["u1", "a1", "u2"]);
    }

    #[test]
    fn unchanged_history_writes_nothing_and_a_rewind_writes_a_truncate() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with(&["u1", "a1", "u2", "a2"]);
        let journal = SessionJournal::new(dir.path(), &session);
        journal.sync_messages(&session.messages).unwrap();
        let written = journal_lines(dir.path(), &session.id).len();
        journal.sync_messages(&session.messages).unwrap();
        assert_eq!(journal_lines(dir.path(), &session.id).len(), written);

        session.messages.truncate(1);
        session.add_message(Message::assistant("a1-retry"));
        journal.sync_messages(&session.messages).unwrap();
        drop(journal);
        let loaded = load(dir.path(), &session.id).unwrap();
        assert_eq!(texts(&loaded.session.messages), ["u1", "a1-retry"]);
    }

    #[test]
    fn compaction_entry_round_trips_with_the_replaced_messages() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with(&["u1", "a1", "u2", "a2"]);
        let journal = SessionJournal::new(dir.path(), &session);
        journal.checkpoint(&mut session).unwrap();
        let record = compact(&mut session, 2, "summary");
        assert_eq!(record.replaced_len(), 2);
        journal.record_compaction(&record, &session.messages).unwrap();
        session.compactions.push(record);

        // From the journal (no checkpoint since the compaction).
        let loaded = load(dir.path(), &session.id).unwrap();
        assert_eq!(texts(&loaded.session.messages), ["summary", "u2", "a2"]);
        assert_eq!(loaded.session.compactions.len(), 1);
        assert_eq!(texts(&loaded.session.compactions[0].replaced), ["u1", "a1"]);

        // From the snapshot.
        journal.checkpoint(&mut session).unwrap();
        drop(journal);
        let loaded = load(dir.path(), &session.id).unwrap();
        assert_eq!(loaded.replayed, 0);
        assert_eq!(texts(&loaded.session.compactions[0].replaced), ["u1", "a1"]);
    }

    #[test]
    fn resume_after_compactions_starts_from_the_latest_summary() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with(&["u1", "a1", "u2", "a2"]);
        let journal = SessionJournal::new(dir.path(), &session);
        let first = compact(&mut session, 2, "summary-1");
        journal.record_compaction(&first, &session.messages).unwrap();
        session.compactions.push(first);
        session.add_message(Message::user("u3"));
        session.add_message(Message::assistant("a3"));
        journal.sync_messages(&session.messages).unwrap();
        let second = compact(&mut session, 3, "summary-2");
        journal.record_compaction(&second, &session.messages).unwrap();
        session.compactions.push(second);
        drop(journal);

        let loaded = load(dir.path(), &session.id).unwrap();
        assert!(loaded.rebuilt);
        let resumed = loaded.session;
        assert_eq!(texts(&resumed.messages), ["summary-2", "u3", "a3"]);
        assert_eq!(texts(&resumed.compactions[1].replaced), ["summary-1", "u2", "a2"]);
        let timeline: Vec<String> = resumed.rewind_timeline().map(|m| m.get_all_text()).collect();
        assert_eq!(
            timeline,
            ["u1", "a1", "summary-1", "u2", "a2", "summary-2", "u3", "a3"]
        );
    }

    #[test]
    fn rewind_past_a_compaction_restores_and_persists_the_earlier_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with(&["u1", "a1", "u2", "a2"]);
        let journal = SessionJournal::new(dir.path(), &session);
        journal.checkpoint(&mut session).unwrap();
        let record = compact(&mut session, 2, "summary");
        journal.record_compaction(&record, &session.messages).unwrap();
        session.compactions.push(record);
        journal.checkpoint(&mut session).unwrap();

        // Position 1 is "a1", inside the replaced head.
        session.rewind_to(1);
        assert_eq!(texts(&session.messages), ["u1"]);
        assert!(session.compactions.is_empty());
        journal.checkpoint(&mut session).unwrap();
        drop(journal);

        let loaded = load(dir.path(), &session.id).unwrap();
        assert_eq!(texts(&loaded.session.messages), ["u1"]);
        assert!(loaded.session.compactions.is_empty());
        std::fs::remove_file(snapshot_path(dir.path(), &session.id)).unwrap();
        let rebuilt = load(dir.path(), &session.id).unwrap();
        assert_eq!(texts(&rebuilt.session.messages), ["u1"]);
        assert!(rebuilt.session.compactions.is_empty());
    }

    #[test]
    fn rewind_inside_the_live_history_keeps_compactions() {
        let mut session = session_with(&["u1", "a1", "u2", "a2"]);
        let record = compact(&mut session, 2, "summary");
        session.compactions.push(record);
        // Timeline: u1 a1 | summary u2 a2; position 4 is "a2".
        session.rewind_to(4);
        assert_eq!(texts(&session.messages), ["summary", "u2"]);
        assert_eq!(session.compactions.len(), 1);
    }

    #[test]
    fn a_second_writer_is_locked_out_until_the_first_drops() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with(&["u1"]);
        let first = SessionJournal::new(dir.path(), &session);
        first.sync_messages(&session.messages).unwrap();

        let second = SessionJournal::new(dir.path(), &session);
        let err = second.sync_messages(&session.messages).unwrap_err();
        assert!(err.downcast_ref::<SessionLocked>().is_some(), "{err}");
        assert!(delete(dir.path(), &session.id).is_err());

        drop(first);
        session.add_message(Message::assistant("a1"));
        second.sync_messages(&session.messages).unwrap();
        drop(second);
        let loaded = load(dir.path(), &session.id).unwrap();
        assert_eq!(texts(&loaded.session.messages), ["u1", "a1"]);
    }

    #[test]
    fn save_goes_through_the_live_writer_of_this_process() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with(&["u1"]);
        let live = SessionJournal::new(dir.path(), &session);
        live.acquire().unwrap();
        session.title = Some("renamed".to_string());
        save(dir.path(), &mut session).unwrap();
        drop(live);
        let loaded = load(dir.path(), &session.id).unwrap();
        assert_eq!(loaded.session.title.as_deref(), Some("renamed"));
    }

    #[test]
    fn pre_journal_snapshot_loads_and_starts_a_full_journal_when_saved() {
        let dir = tempfile::tempdir().unwrap();
        let id = "11111111-2222-3333-4444-555555555555";
        // The shape `history::save_session` wrote before issue #449.
        let old = format!(
            r#"{{
  "id": "{id}",
  "created_at": "2026-10-01T10:00:00Z",
  "updated_at": "2026-10-01T10:05:00Z",
  "messages": [
    {{"role": "user", "content": "hello"}},
    {{"role": "assistant", "content": [{{"type": "text", "text": "hi"}}]}}
  ],
  "model": "claude-sonnet",
  "title": "old",
  "working_dir": "/tmp",
  "tags": [],
  "total_cost": 0.0,
  "total_tokens": 0,
  "checkpoints": []
}}"#
        );
        std::fs::write(snapshot_path(dir.path(), id), old).unwrap();

        let mut session = load(dir.path(), id).unwrap().session;
        assert_eq!(texts(&session.messages), ["hello", "hi"]);
        assert_eq!(session.journal_seq, 0);

        save(dir.path(), &mut session).unwrap();
        std::fs::remove_file(snapshot_path(dir.path(), id)).unwrap();
        let rebuilt = load(dir.path(), id).unwrap();
        assert_eq!(texts(&rebuilt.session.messages), ["hello", "hi"]);
        assert_eq!(rebuilt.session.title.as_deref(), Some("old"));
    }

    /// A journal started beside a pre-journal snapshot (journal_seq 0), with
    /// a crash before the turn's checkpoint, replays to the live history, not
    /// the snapshot's history plus a second copy.
    #[test]
    fn journal_started_beside_a_legacy_snapshot_replays_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let mut legacy = session_with(&["u1", "a1"]);
        legacy.journal_seq = 0;
        std::fs::write(
            snapshot_path(dir.path(), &legacy.id),
            serde_json::to_vec_pretty(&legacy).unwrap(),
        )
        .unwrap();

        let mut session = load(dir.path(), &legacy.id).unwrap().session;
        let journal = SessionJournal::new(dir.path(), &session);
        session.add_message(Message::user("u2"));
        journal.sync_messages(&session.messages).unwrap();
        drop(journal);

        let loaded = load(dir.path(), &legacy.id).unwrap();
        assert!(!loaded.rebuilt);
        assert_eq!(texts(&loaded.session.messages), ["u1", "a1", "u2"]);
    }

    /// Same when the journal file is lost after a checkpoint: the new journal
    /// is numbered above the snapshot and replaces its history on replay.
    #[test]
    fn lost_journal_is_restarted_without_duplicating_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with(&["u1", "a1"]);
        save(dir.path(), &mut session).unwrap();
        std::fs::remove_file(journal_path(dir.path(), &session.id)).unwrap();

        let mut session = load(dir.path(), &session.id).unwrap().session;
        let journal = SessionJournal::new(dir.path(), &session);
        session.add_message(Message::user("u2"));
        journal.sync_messages(&session.messages).unwrap();
        drop(journal);

        let loaded = load(dir.path(), &session.id).unwrap();
        assert!(!loaded.rebuilt);
        assert_eq!(texts(&loaded.session.messages), ["u1", "a1", "u2"]);
        std::fs::remove_file(snapshot_path(dir.path(), &session.id)).unwrap();
        let rebuilt = load(dir.path(), &session.id).unwrap();
        assert_eq!(texts(&rebuilt.session.messages), ["u1", "a1", "u2"]);
    }

    /// Every session field outside the history is carried by `meta` entries,
    /// so a journal-only rebuild does not silently drop a field added later.
    #[test]
    fn session_meta_covers_every_session_field_but_the_history() {
        let mut session = session_with(&["u1"]);
        // Fill every optional field so it serialises.
        session.branch_from = Some("b".to_string());
        session.branch_at_message = Some(1);
        session.remote_session_url = Some("u".to_string());
        session.parent_session_id = Some("p".to_string());
        session.fork_point_message_index = Some(1);
        session.working_dir = Some("/w".to_string());
        session.tags = vec!["t".to_string()];
        let keys = |value: serde_json::Value| -> BTreeSet<String> {
            value.as_object().unwrap().keys().cloned().collect()
        };
        let mut session_keys = keys(serde_json::to_value(&session).unwrap());
        for history in ["messages", "checkpoints", "compactions", "journal_seq"] {
            session_keys.remove(history);
        }
        let meta = SessionMeta::of(&session);
        assert_eq!(session_keys, keys(serde_json::to_value(&meta).unwrap()));

        let mut rebuilt = ConversationSession::new(String::new());
        meta.apply(&mut rebuilt);
        rebuilt.messages = session.messages.clone();
        assert_eq!(
            serde_json::to_value(&rebuilt).unwrap(),
            serde_json::to_value(&session).unwrap()
        );
    }

    #[test]
    fn missing_session_is_an_io_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let err = load(dir.path(), "absent").err().unwrap();
        let io = err.downcast_ref::<std::io::Error>().unwrap();
        assert_eq!(io.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn delete_removes_every_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session_with(&["u1"]);
        save(dir.path(), &mut session).unwrap();
        delete(dir.path(), &session.id).unwrap();
        assert!(std::fs::read_dir(dir.path()).unwrap().next().is_none());
    }
}
