//! Canonical filesystem locations for claurst.
//!
//! Everything claurst persists lives under a single root directory. This module
//! exposes the one resolver ([`claurst_home`]) that the whole workspace routes
//! through, so the home-dir precedence (see [`crate::config::Settings::config_dir`])
//! is defined in exactly one place.

use std::path::PathBuf;

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
