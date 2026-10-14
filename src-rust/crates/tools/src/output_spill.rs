// Bounded capture of shell output with spill-to-file (#442).
//
// A command's output is drained to the end no matter how large it is: the
// accumulator keeps a fixed head window and a rolling tail in memory and drops
// the middle, so a chatty command can never fill the pty/pipe, block, and time
// out with its output lost. Bytes leaving the middle are written to a private
// spill file under `<claurst home>/tool-output/`, so the full output stays
// available to the model via `Full output: <path>`.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::{Duration, SystemTime};
use tracing::debug;

/// Raw bytes kept from the start of the stream.
pub(crate) const RAW_HEAD_BYTES: usize = 64 * 1024;
/// Raw bytes kept from the end of the stream. Larger than the tail window the
/// model sees because it must also hold the PTY wrapper's shell-state block
/// (`pwd` + `env`), which is split off before rendering.
pub(crate) const RAW_TAIL_BYTES: usize = 1024 * 1024;
/// Spill files older than this are deleted by the once-per-process sweep.
pub(crate) const SPILL_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Collects a byte stream into a head window plus a rolling tail, spilling the
/// full stream to a file once the middle starts being dropped.
pub(crate) struct OutputAccumulator {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    head_cap: usize,
    tail_cap: usize,
    omitted: u64,
    /// Where to spill; taken when the spill file is first created (or fails).
    spill_path: Option<PathBuf>,
    spill: Option<SpillFile>,
}

/// What an [`OutputAccumulator`] retained once the stream ended.
#[derive(Debug)]
pub(crate) enum Captured {
    /// Nothing was dropped; these are all the bytes.
    Complete(Vec<u8>),
    /// The middle `omitted` bytes were dropped between `head` and `tail`.
    /// `spill`, when present, holds `head` plus the dropped bytes; the caller
    /// appends the part of `tail` it wants kept with [`finish_spill`]. Writing
    /// the tail last lets the PTY path leave its shell-state block (which
    /// carries the environment) off disk.
    Truncated {
        // Only the PTY renderer (Unix) reads `head` and `omitted`; the
        // background path needs just `tail` and `spill`.
        #[cfg_attr(not(unix), allow(dead_code))]
        head: Vec<u8>,
        tail: Vec<u8>,
        #[cfg_attr(not(unix), allow(dead_code))]
        omitted: u64,
        spill: Option<SpillFile>,
    },
}

impl OutputAccumulator {
    pub(crate) fn new(head_cap: usize, tail_cap: usize, spill_path: Option<PathBuf>) -> Self {
        Self {
            head: Vec::new(),
            tail: VecDeque::new(),
            head_cap,
            tail_cap,
            omitted: 0,
            spill_path,
            spill: None,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        let room = self.head_cap.saturating_sub(self.head.len()).min(bytes.len());
        let (head, rest) = bytes.split_at(room);
        self.head.extend_from_slice(head);
        self.tail.extend(rest);
        let excess = self.tail.len().saturating_sub(self.tail_cap);
        if excess == 0 {
            return;
        }
        if let Some(path) = self.spill_path.take() {
            self.spill = SpillFile::create(&path)
                .and_then(|mut spill| spill.write(&self.head).map(|()| spill))
                .map_err(|e| debug!(path = %path.display(), error = %e, "spill file unavailable"))
                .ok();
        }
        if let Some(spill) = self.spill.as_mut() {
            // The evicted bytes are the front of the ring, possibly split
            // across its two slices.
            let (front, back) = self.tail.as_slices();
            let in_front = excess.min(front.len());
            let written = spill
                .write(&front[..in_front])
                .and_then(|()| spill.write(&back[..excess - in_front]));
            if let Err(e) = written {
                debug!(path = %spill.path.display(), error = %e, "spill write failed");
                self.spill = None;
            }
        }
        self.tail.drain(..excess);
        self.omitted += excess as u64;
    }

    pub(crate) fn finish(self) -> Captured {
        let Self {
            mut head,
            tail,
            omitted,
            spill,
            ..
        } = self;
        if omitted == 0 {
            head.extend(tail);
            Captured::Complete(head)
        } else {
            Captured::Truncated {
                head,
                tail: tail.into(),
                omitted,
                spill,
            }
        }
    }
}

/// A private (0600) spill file being written.
#[derive(Debug)]
pub(crate) struct SpillFile {
    path: PathBuf,
    file: BufWriter<File>,
}

impl SpillFile {
    /// Create `path` readable only by the owner, never clobbering an existing
    /// file. The first call in a process also sweeps stale spill files from the
    /// same directory.
    fn create(path: &Path) -> std::io::Result<Self> {
        static SWEEP: Once = Once::new();
        if let Some(dir) = path.parent() {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(dir)?;
            SWEEP.call_once(|| sweep_spill_dir(dir, SPILL_MAX_AGE));
        }
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        Ok(Self {
            path: path.to_path_buf(),
            file: BufWriter::new(options.open(path)?),
        })
    }

    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.file.write_all(bytes)
    }
}

/// Append `rest` to `spill` and close it, returning its path. `None` when there
/// is no spill file or writing fails (logged; the result just has no path).
pub(crate) fn finish_spill(spill: Option<SpillFile>, rest: &[u8]) -> Option<PathBuf> {
    let mut spill = spill?;
    match spill.write(rest).and_then(|()| spill.file.flush()) {
        Ok(()) => Some(spill.path),
        Err(e) => {
            debug!(path = %spill.path.display(), error = %e, "spill write failed");
            None
        }
    }
}

/// Write `bytes` as a complete spill file at `path`; `None` on failure (logged).
pub(crate) fn spill_bytes(path: &Path, bytes: &[u8]) -> Option<PathBuf> {
    match SpillFile::create(path) {
        Ok(spill) => finish_spill(Some(spill), bytes),
        Err(e) => {
            debug!(path = %path.display(), error = %e, "spill file unavailable");
            None
        }
    }
}

/// Append the `Full output: <path>` trailer when the output was spilled.
pub(crate) fn with_full_output(mut text: String, spill: Option<PathBuf>) -> String {
    if let Some(path) = spill {
        text.push_str(&format!("\n\nFull output: {}", path.display()));
    }
    text
}

/// Delete regular files in `dir` last modified more than `max_age` ago.
/// Best effort: unreadable entries are skipped.
pub(crate) fn sweep_spill_dir(dir: &Path, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let stale = meta.is_file()
            && meta
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_some_and(|age| age > max_age);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_stream_is_complete_and_never_spills() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let mut acc = OutputAccumulator::new(4, 4, Some(path.clone()));
        acc.push(b"abc");
        acc.push(b"def");
        match acc.finish() {
            Captured::Complete(bytes) => assert_eq!(bytes, b"abcdef"),
            other => panic!("expected Complete, got {other:?}"),
        }
        assert!(!path.exists());
    }

    #[test]
    fn large_stream_keeps_head_and_rolling_tail_and_spills_everything() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool-output").join("out.txt");
        let mut acc = OutputAccumulator::new(4, 4, Some(path.clone()));
        for chunk in [&b"0123"[..], b"4567", b"89ab", b"cdef"] {
            acc.push(chunk);
        }
        match acc.finish() {
            Captured::Truncated {
                head,
                tail,
                omitted,
                spill,
            } => {
                assert_eq!(head, b"0123");
                assert_eq!(tail, b"cdef");
                assert_eq!(omitted, 8);
                assert_eq!(finish_spill(spill, &tail[..2]), Some(path.clone()));
            }
            other => panic!("expected Truncated, got {other:?}"),
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"0123456789abcd");
    }

    #[cfg(unix)]
    #[test]
    fn spill_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = spill_bytes(&dir.path().join("out.txt"), b"secret").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn spill_never_clobbers_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        std::fs::write(&path, b"keep").unwrap();
        assert_eq!(spill_bytes(&path, b"new"), None);
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
    }

    #[test]
    fn trailer_names_the_spill_path() {
        let path = PathBuf::from("/x/tool-output/a.txt");
        assert_eq!(with_full_output("out".into(), None), "out");
        assert_eq!(
            with_full_output("out".into(), Some(path.clone())),
            format!("out\n\nFull output: {}", path.display())
        );
    }

    #[test]
    fn sweep_deletes_only_stale_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("old.txt");
        std::fs::write(&file, b"x").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();

        sweep_spill_dir(dir.path(), SPILL_MAX_AGE);
        assert!(file.exists(), "fresh file must survive");

        std::thread::sleep(Duration::from_millis(20));
        sweep_spill_dir(dir.path(), Duration::from_millis(1));
        assert!(!file.exists(), "stale file must be deleted");
        assert!(dir.path().join("subdir").exists(), "directories are left alone");
    }
}
