//! Workspace root types and name generation for named workspace roots.
//!
//! A workspace root is a named, absolute directory that Claurst can access.
//! The primary root is always named `"main"` and corresponds to the session's
//! working directory. Additional roots come from `--add-dir` CLI flags and
//! persisted `workspace_paths` in settings.json.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

/// A named workspace root.
#[derive(Debug, Clone)]
pub struct WorkspaceRoot {
    /// Stable name (e.g. "main", "report-a", "lib-2")
    pub name: String,
    /// Absolute path
    pub path: PathBuf,
    /// Whether this is the primary (main) working directory
    pub is_primary: bool,
}

/// Result of resolving a path that may use `&root-name` syntax.
#[derive(Debug, Clone)]
pub struct ResolvedWorkspacePath {
    /// The original input string.
    pub input: String,
    /// Root name if `&root-name/path` syntax was used.
    pub root_name: Option<String>,
    /// Absolute path of the matched root.
    pub root_path: Option<PathBuf>,
    /// Path fragment relative to the root.
    pub relative_path: Option<PathBuf>,
    /// Final resolved absolute path.
    pub absolute_path: PathBuf,
}

/// Parse an input string that may use `&root-name/relative-path` syntax.
///
/// Returns `None` when the input is an absolute path, a plain relative path
/// (no `&` prefix), or when the `&` prefix names an unknown root.
///
/// When the input is `&root-name` alone (no slash), returns the root path
/// itself (empty relative path).
///
/// # Resolution order
/// 1. Absolute path (`C:\...`, `/home/...`) → `None` (not &-syntax)
/// 2. `&root-name/sub/path` → `Some((root_name, "sub/path"))`
/// 3. `&root-name` → `Some((root_name, ""))`
/// 4. Plain relative path → `None`
pub fn parse_root_ref<'a>(input: &'a str, roots: &BTreeMap<String, PathBuf>) -> Option<(&'a str, &'a str)> {
    if std::path::Path::new(input).is_absolute() {
        return None;
    }
    if !input.starts_with('&') {
        return None;
    }
    let without_prefix = &input[1..];
    if let Some(slash_pos) = without_prefix.find(|c| c == '/' || c == '\\') {
        let candidate_root = &without_prefix[..slash_pos];
        if roots.contains_key(candidate_root) {
            let remainder = &without_prefix[slash_pos + 1..];
            return Some((candidate_root, remainder));
        }
    } else if roots.contains_key(without_prefix) {
        return Some((without_prefix, ""));
    }
    None
}

/// Generate a `BTreeMap<String, PathBuf>` of workspace roots from the primary
/// working directory and any additional directories.
///
/// The primary directory is always named `"main"`. Additional paths are named
/// after a sanitized version of their last path component, with numeric suffixes
/// appended for duplicates (e.g. `"lib"`, `"lib-2"`, `"lib-3"`).
///
/// # Panics
/// Panics if `primary` is not an absolute path.
pub fn generate_root_names(
    primary: &std::path::Path,
    additional_dirs: &[PathBuf],
    workspace_paths: &[PathBuf],
) -> BTreeMap<String, PathBuf> {
    assert!(
        primary.is_absolute(),
        "workspace primary root must be an absolute path, got: {}",
        primary.display()
    );

    let mut result = BTreeMap::new();
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();
    let mut seen_names: HashSet<String> = HashSet::new();

    // 1. Primary root — always named "main"
    result.insert("main".to_string(), primary.to_path_buf());
    seen_paths.insert(canonicalize_or_raw(primary));
    seen_names.insert("main".to_string());

    // 2. Additional directories
    let all_extra: Vec<&PathBuf> = additional_dirs
        .iter()
        .chain(workspace_paths.iter())
        .collect();

    for path in all_extra {
        let canon = canonicalize_or_raw(path);

        // Skip if this exact path is already registered
        if seen_paths.contains(&canon) {
            continue;
        }
        seen_paths.insert(canon.clone());

        // Generate a stable, prompt-friendly name from the last path component.
        let base_name = path
            .file_name()
            .map(|n| sanitize_root_name(&n.to_string_lossy()))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "workspace".to_string());

        let mut name = base_name.clone();
        let mut counter = 2;
        while !seen_names.insert(name.clone()) {
            name = format!("{}-{}", base_name, counter);
            counter += 1;
        }

        result.insert(name, path.clone());
    }

    result
}

fn canonicalize_or_raw(path: &std::path::Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn sanitize_root_name(name: &str) -> String {
    let mut out = String::new();
    let mut previous_dash = false;

    for ch in name.chars().flat_map(|ch| ch.to_lowercase()) {
        let mapped = if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            ch
        } else {
            '-'
        };

        if mapped == '-' {
            if previous_dash {
                continue;
            }
            previous_dash = true;
        } else {
            previous_dash = false;
        }
        out.push(mapped);
    }

    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "workspace".to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Return an absolute path suitable for tests on any platform.
    fn abs_workspace() -> PathBuf {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/workspace"))
    }

    #[test]
    #[should_panic(expected = "workspace primary root must be an absolute path")]
    fn rejects_relative_primary() {
        generate_root_names(
            std::path::Path::new("relative/path"),
            &[],
            &[],
        );
    }

    #[test]
    fn primary_is_main() {
        let base = abs_workspace();
        let roots = generate_root_names(&base, &[], &[]);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots.get("main").unwrap(), &base);
    }

    #[test]
    fn additional_dirs_get_name_from_component() {
        let base = abs_workspace();
        let extra = PathBuf::from(&base).join("report-a");
        let roots = generate_root_names(&base, &[extra.clone()], &[]);
        assert_eq!(roots.len(), 2);
        assert_eq!(roots.get("report-a").unwrap(), &extra);
    }

    #[test]
    fn duplicate_names_get_numeric_suffix() {
        let base = abs_workspace();
        let dir_a = PathBuf::from(&base).join("projects").join("lib");
        let dir_b = PathBuf::from(&base).join("extern").join("lib");
        let roots = generate_root_names(&base, &[dir_a.clone(), dir_b.clone()], &[]);
        assert_eq!(roots.len(), 3);
        assert_eq!(roots.get("lib").unwrap(), &dir_a);
        assert_eq!(roots.get("lib-2").unwrap(), &dir_b);
    }

    #[test]
    fn duplicate_paths_are_skipped() {
        let base = abs_workspace();
        let lib = PathBuf::from(&base).join("lib");
        let roots = generate_root_names(&base, &[lib.clone(), lib.clone()], &[]);
        assert_eq!(roots.len(), 2);
    }

    #[test]
    fn path_in_additional_and_workspace_paths_deduped() {
        let base = abs_workspace();
        let lib = PathBuf::from(&base).join("lib");
        let roots = generate_root_names(&base, &[lib.clone()], &[lib.clone()]);
        assert_eq!(roots.len(), 2);
    }

    #[test]
    fn names_are_case_insensitive_for_dedup() {
        let base = abs_workspace();
        let lib_a = PathBuf::from(&base).join("Lib");
        let lib_b = PathBuf::from(&base).join("extern").join("lib");
        let roots = generate_root_names(&base, &[lib_a.clone(), lib_b.clone()], &[]);
        assert_eq!(roots.get("lib-2").unwrap(), &lib_b);
    }

    #[test]
    fn root_names_are_sanitized_for_prompt_references() {
        let base = abs_workspace();
        let fancy = PathBuf::from(&base).join("My Project (API)");
        let unicode = PathBuf::from(&base).join("剧本创意体系");
        let roots = generate_root_names(&base, &[fancy.clone(), unicode.clone()], &[]);
        assert_eq!(roots.get("my-project-api").unwrap(), &fancy);
        assert_eq!(roots.get("workspace").unwrap(), &unicode);
    }

    #[test]
    fn parse_root_ref_absolute_path_returns_none() {
        let roots = BTreeMap::from([("main".to_string(), PathBuf::from("/workspace"))]);
        assert!(parse_root_ref("C:\\Users\\test.rs", &roots).is_none());
        assert!(parse_root_ref("D:\\abs\\path", &roots).is_none());
    }

    #[test]
    fn parse_root_ref_plain_relative_returns_none() {
        let roots = BTreeMap::from([("main".to_string(), PathBuf::from("/workspace"))]);
        assert!(parse_root_ref("src/main.rs", &roots).is_none());
    }

    #[test]
    fn parse_root_ref_with_subpath() {
        let roots = BTreeMap::from([
            ("main".to_string(), PathBuf::from("/workspace")),
            ("lib".to_string(), PathBuf::from("/lib")),
        ]);
        let result = parse_root_ref("&lib/src/main.rs", &roots);
        assert_eq!(result, Some(("lib", "src/main.rs")));
    }

    #[test]
    fn parse_root_ref_alone_returns_empty_subpath() {
        let roots = BTreeMap::from([("main".to_string(), PathBuf::from("/workspace"))]);
        let result = parse_root_ref("&main", &roots);
        assert_eq!(result, Some(("main", "")));
    }

    #[test]
    fn parse_root_ref_unknown_root_returns_none() {
        let roots = BTreeMap::from([("main".to_string(), PathBuf::from("/workspace"))]);
        assert!(parse_root_ref("&unknown/path.rs", &roots).is_none());
    }

    #[test]
    fn parse_root_ref_with_backslash_on_windows() {
        let roots = BTreeMap::from([("lib".to_string(), PathBuf::from("/lib"))]);
        let result = parse_root_ref("&lib\\src\\main.rs", &roots);
        assert_eq!(result, Some(("lib", "src\\main.rs")));
    }
}
