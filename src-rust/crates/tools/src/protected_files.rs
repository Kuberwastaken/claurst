//! File tools must not rewrite the settings and credentials that govern them.
//! Trusted settings/auth commands still use their own persistence APIs.

use std::path::{Component, Path, PathBuf};

/// Resolve existing ancestors before folding the missing suffix, so both
/// symlink aliases and not-yet-created credential files are covered.
fn resolved(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let joined = absolute
        .ancestors()
        .find_map(|ancestor| {
            let real = std::fs::canonicalize(ancestor).ok()?;
            Some(real.join(absolute.strip_prefix(ancestor).ok()?))
        })
        .unwrap_or(absolute);
    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            #[cfg(windows)]
            Component::Normal(name) => {
                // Win32 paths are case-insensitive and trim trailing dots and
                // spaces; an alternate data stream still belongs to its file.
                let name = name.to_string_lossy();
                let base = name.split(':').next().unwrap_or(&name);
                normalized.push(base.trim_end_matches(['.', ' ']).to_lowercase());
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

pub(crate) fn is_protected(path: &Path, home: &Path, mcp_tokens: Option<&Path>) -> bool {
    let path = resolved(path);
    for name in [
        "settings.json",
        "mcp_trust.json",
        "auth.json",
        "accounts.json",
        "oauth_tokens.json",
        "codex_tokens.json",
        "accounts",
        "mcp-tokens",
    ] {
        let protected = resolved(&home.join(name));
        if path == protected || path.starts_with(&protected) {
            return true;
        }
    }
    // Project approval rules also govern future tool execution.
    if path.starts_with(resolved(home))
        && path
            .file_name()
            .is_some_and(|name| name == "permissions.json")
    {
        return true;
    }
    mcp_tokens.is_some_and(|tokens| path.starts_with(resolved(tokens)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protects_missing_settings_and_nested_credentials_but_not_user_data() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("claurst");
        std::fs::create_dir(&home).unwrap();
        for name in [
            "settings.json",
            "mcp_trust.json",
            "auth.json",
            "accounts/codex/profile/codex_tokens.json",
            "projects/project/permissions.json",
            "mcp-tokens/server.json",
        ] {
            assert!(is_protected(&home.join(name), &home, None), "{name}");
        }
        for name in [
            "memory/MEMORY.md",
            "tool-output/result.txt",
            "sessions/session.json",
            "settings.json.example",
        ] {
            assert!(!is_protected(&home.join(name), &home, None), "{name}");
        }
        assert!(!is_protected(
            &root.path().join("settings.json"),
            &home,
            None
        ));
        assert!(is_protected(
            &home.join("unused/../settings.json"),
            &home,
            None
        ));
        let override_dir = root.path().join("tokens");
        assert!(is_protected(
            &override_dir.join("server.json"),
            &home,
            Some(&override_dir)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn follows_existing_directory_and_file_aliases() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("claurst");
        std::fs::create_dir(&home).unwrap();
        std::fs::write(home.join("settings.json"), "{}").unwrap();
        std::fs::write(home.join("mcp_trust.json"), "{}").unwrap();
        assert!(is_protected(&home.join("mcp_trust.json"), &home, None));
        let trust_alias = root.path().join("trust.txt");
        std::os::unix::fs::symlink(home.join("mcp_trust.json"), &trust_alias).unwrap();
        assert!(is_protected(&trust_alias, &home, None));
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&home, &alias).unwrap();
        assert!(is_protected(&alias.join("settings.json"), &home, None));
        assert!(is_protected(
            &alias.join("accounts/new/auth.json"),
            &home,
            None
        ));
        let file = root.path().join("ordinary.txt");
        std::os::unix::fs::symlink(home.join("settings.json"), &file).unwrap();
        assert!(is_protected(&file, &home, None));
    }

    #[cfg(windows)]
    #[test]
    fn windows_case_dot_and_stream_aliases_are_protected() {
        let root = tempfile::tempdir().unwrap();
        for name in ["SETTINGS.JSON", "auth.json. ", "settings.json:stream"] {
            assert!(
                is_protected(&root.path().join(name), root.path(), None),
                "{name}"
            );
        }
    }
}
