//! Canonical filesystem locations for claurst.
//!
//! Everything claurst persists lives under a single root directory. This module
//! exposes the one resolver ([`claurst_home`]) that the whole workspace routes
//! through, so the home-dir precedence (see [`crate::config::Settings::config_dir`])
//! is defined in exactly one place.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

/// The canonical claurst home directory — the single source of truth for where
/// claurst keeps its data. Thin wrapper over
/// [`crate::config::Settings::config_dir`]; prefer this at call sites that only
/// need the root path.
///
/// Resolution precedence (issue #207 — XDG support, back-compatible):
/// 1. `$CLAURST_HOME` if set and non-empty (verbatim).
/// 2. Legacy `~/.claurst` if it already exists.
/// 3. `$XDG_CONFIG_HOME/claurst` (when absolute) else `~/.config/claurst`.
pub fn claurst_home() -> PathBuf {
    crate::config::Settings::config_dir()
}

/// Where oversized tool output is spilled for the model to read back:
/// `<claurst_home>/tool-output/` (#442).
pub fn tool_output_dir() -> PathBuf {
    claurst_home().join("tool-output")
}

/// Size at which the diagnostic log is rotated to `claurst.log.old` on open.
pub const LOG_FILE_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// Where the interactive TUI writes `tracing` output: `<claurst_home>/logs/claurst.log`.
/// The TUI owns the terminal, so log lines must never reach stdout/stderr (#396).
pub fn log_file_path() -> PathBuf {
    log_file_path_in(&claurst_home())
}

/// [`log_file_path`] rooted at an explicit home directory.
pub fn log_file_path_in(home: &Path) -> PathBuf {
    home.join("logs").join("claurst.log")
}

/// Whether a log file of `current_len` bytes should be rotated before appending.
pub fn log_needs_rotation(current_len: u64) -> bool {
    current_len >= LOG_FILE_MAX_BYTES
}

/// Open `path` for appending, creating its parent directory. When the existing
/// file has reached [`LOG_FILE_MAX_BYTES`] it is first renamed to
/// `<name>.old` (replacing any previous one), so the log keeps at most two
/// generations on disk.
pub fn open_log_file(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Ok(meta) = std::fs::metadata(path) {
        if log_needs_rotation(meta.len()) {
            let mut rotated = path.as_os_str().to_owned();
            rotated.push(".old");
            std::fs::rename(path, PathBuf::from(rotated))?;
        }
    }
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(test)]
mod log_file_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn log_file_lives_under_home_logs_dir() {
        let home = Path::new("/tmp/claurst-home");
        assert_eq!(
            log_file_path_in(home),
            home.join("logs").join("claurst.log")
        );
    }

    #[test]
    fn rotation_threshold() {
        assert!(!log_needs_rotation(0));
        assert!(!log_needs_rotation(LOG_FILE_MAX_BYTES - 1));
        assert!(log_needs_rotation(LOG_FILE_MAX_BYTES));
    }

    #[test]
    fn open_creates_dir_and_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let path = log_file_path_in(tmp.path());
        writeln!(open_log_file(&path).unwrap(), "one").unwrap();
        writeln!(open_log_file(&path).unwrap(), "two").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\n");
    }

    #[test]
    fn open_rotates_oversized_log() {
        let tmp = tempfile::tempdir().unwrap();
        let path = log_file_path_in(tmp.path());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![b'x'; LOG_FILE_MAX_BYTES as usize]).unwrap();

        writeln!(open_log_file(&path).unwrap(), "fresh").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh\n");
        let old = path.with_file_name("claurst.log.old");
        assert_eq!(std::fs::metadata(old).unwrap().len(), LOG_FILE_MAX_BYTES);
    }
}

// These tests drive the resolver through `HOME`/`XDG_CONFIG_HOME`, which only
// govern `dirs::home_dir()` on Unix — on Windows the home dir comes from the OS
// profile API and can't be pinned via env, so they'd be non-hermetic there.
#[cfg(all(test, unix))]
mod tests {
    use crate::config::Settings;
    use std::path::PathBuf;
    use std::sync::Mutex;

    // The resolver reads process-global env (`CLAURST_HOME`, `HOME`,
    // `XDG_CONFIG_HOME`). Serialize every test that mutates them.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvGuard {
        fn new() -> Self {
            let keys = ["CLAURST_HOME", "HOME", "XDG_CONFIG_HOME"];
            let saved = keys
                .iter()
                .map(|k| (*k, std::env::var_os(k)))
                .collect::<Vec<_>>();
            for k in keys {
                std::env::remove_var(k);
            }
            Self { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[test]
    fn claurst_home_env_override_wins() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        // Set HOME + an existing legacy dir + XDG too, to prove the override
        // takes precedence over every other rule and is used verbatim.
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claurst")).unwrap();
        std::env::set_var("HOME", home.path());
        std::env::set_var("XDG_CONFIG_HOME", home.path());
        std::env::set_var("CLAURST_HOME", tmp.path());

        assert_eq!(Settings::config_dir(), tmp.path());
    }

    #[test]
    fn claurst_home_empty_env_override_ignored() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        std::env::set_var("CLAURST_HOME", "");

        // Empty override falls through to XDG (no legacy dir, no XDG_CONFIG_HOME).
        assert_eq!(
            Settings::config_dir(),
            home.path().join(".config").join("claurst")
        );
    }

    #[test]
    fn claurst_home_legacy_dir_used_when_present() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        let legacy = home.path().join(".claurst");
        std::fs::create_dir_all(&legacy).unwrap();
        std::env::set_var("HOME", home.path());
        // XDG set, but legacy already exists → legacy wins (back-compat).
        std::env::set_var("XDG_CONFIG_HOME", home.path().join("xdg"));

        assert_eq!(Settings::config_dir(), legacy);
    }

    #[test]
    fn claurst_home_xdg_used_when_set_and_no_legacy() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        let xdg = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        std::env::set_var("XDG_CONFIG_HOME", xdg.path());

        assert_eq!(Settings::config_dir(), xdg.path().join("claurst"));
    }

    #[test]
    fn claurst_home_xdg_default_when_no_env() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());

        // No CLAURST_HOME, no legacy dir, no XDG_CONFIG_HOME → ~/.config/claurst.
        assert_eq!(
            Settings::config_dir(),
            home.path().join(".config").join("claurst")
        );
    }

    #[test]
    fn claurst_home_relative_xdg_ignored() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        // Per the XDG spec a relative $XDG_CONFIG_HOME is invalid and ignored.
        std::env::set_var("XDG_CONFIG_HOME", "relative/path");

        assert_eq!(
            Settings::config_dir(),
            home.path().join(".config").join("claurst")
        );
    }

    #[test]
    fn claurst_home_wrapper_matches_config_dir() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("CLAURST_HOME", tmp.path());
        assert_eq!(super::claurst_home(), Settings::config_dir());
        assert_eq!(super::claurst_home(), PathBuf::from(tmp.path()));
    }
}
