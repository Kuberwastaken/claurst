//! Instruction files: `AGENTS.md` / `CLAUDE.md` loaded into the system prompt.
//!
//! Load order (stable; later files are more specific):
//!   1. The global directories (`<claurst home>`, then `~/.claude`)
//!   2. Each project directory from the git root down to `cwd`
//!      (`skill_discovery::project_search_dirs`: never past the git root,
//!      only `cwd` when there is no `.git`)
//!
//! In each directory `AGENTS.override.md`, when present, replaces the other
//! files; otherwise `AGENTS.md`, `CLAUDE.md`, `.claude/CLAUDE.md` and
//! `.claurst/AGENTS.md` are loaded in that order (a file that is the same
//! file, or has the same content, as one already loaded is skipped).
//! YAML frontmatter is stripped and `@include <path>` lines are expanded,
//! confined to the project root (project files) or the global directory
//! the file came from (global files). All files share a
//! [`INSTRUCTION_BUDGET_BYTES`] budget.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// One loaded instruction file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstructionFile {
    /// Path of the file as found on disk.
    pub path: PathBuf,
    /// Content with frontmatter stripped and `@include` lines expanded,
    /// truncated to fit the budget.
    pub content: String,
}

/// Total size of all instruction files, in bytes.
pub const INSTRUCTION_BUDGET_BYTES: usize = 32 * 1024;

/// In a directory that has it, this file replaces `AGENTS.md` and `CLAUDE.md`.
const OVERRIDE_FILE_NAME: &str = "AGENTS.override.md";

/// Loaded in this order when a directory has no override file, as path
/// components relative to the directory. `.claurst/AGENTS.md` is where
/// session memories are persisted; `.claude/CLAUDE.md` is Claude Code's
/// project location.
const INSTRUCTION_FILE_PATHS: [&[&str]; 4] = [
    &["AGENTS.md"],
    &["CLAUDE.md"],
    &[".claude", "CLAUDE.md"],
    &[".claurst", "AGENTS.md"],
];

// ---------------------------------------------------------------------------
// YAML frontmatter
// ---------------------------------------------------------------------------

/// Strip YAML frontmatter (`---` ... `---`) from the start of `content`.
pub fn strip_frontmatter(content: &str) -> &str {
    if let Some(after_first) = content.strip_prefix("---") {
        if let Some(end) = after_first.find("\n---") {
            return after_first[end + 4..].trim_start_matches(['\r', '\n']);
        }
    }
    content
}

// ---------------------------------------------------------------------------
// @include directive expansion
// ---------------------------------------------------------------------------

/// Maximum @include nesting depth.
const MAX_INCLUDE_DEPTH: usize = 10;

/// Largest file `@include` will pull in, in bytes.
const MAX_INCLUDE_BYTES: u64 = 40 * 1024;

/// Expand `@include <path>` lines in `content`.
///
/// Paths are relative to `base_dir` (the including file's directory),
/// absolute, or `~/`-prefixed. A target outside `root` is not read, so a
/// repository's AGENTS.md cannot pull files such as `~/.ssh/...` into the
/// prompt. Circular references are detected via `visited`.
pub fn expand_includes(
    content: &str,
    base_dir: &Path,
    root: &Path,
    visited: &mut HashSet<PathBuf>,
    depth: usize,
) -> String {
    if depth >= MAX_INCLUDE_DEPTH {
        return content.to_string();
    }

    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut result = String::with_capacity(content.len());
    for line in content.lines() {
        let Some(path_str) = line.trim().strip_prefix("@include ") else {
            result.push_str(line);
            result.push('\n');
            continue;
        };
        let path_str = path_str.trim();
        let include_path = match path_str.strip_prefix("~/") {
            Some(rest) => match dirs::home_dir() {
                Some(home) => home.join(rest),
                None => {
                    result.push_str(&format!("<!-- @include {} not found -->\n", path_str));
                    continue;
                }
            },
            None if Path::new(path_str).is_absolute() => PathBuf::from(path_str),
            None => base_dir.join(path_str),
        };

        let Ok(canonical) = include_path.canonicalize() else {
            result.push_str(&format!("<!-- @include {} not found -->\n", path_str));
            continue;
        };
        if !canonical.starts_with(&canonical_root) {
            result.push_str(&format!(
                "<!-- @include {} skipped: outside {} -->\n",
                path_str,
                root.display()
            ));
            continue;
        }
        if visited.contains(&canonical) {
            result.push_str(&format!("<!-- circular @include {} skipped -->\n", path_str));
            continue;
        }
        if std::fs::metadata(&canonical).is_ok_and(|m| m.len() > MAX_INCLUDE_BYTES) {
            result.push_str(&format!("<!-- @include {} exceeds 40KB limit -->\n", path_str));
            continue;
        }
        let Ok(included) = std::fs::read_to_string(&canonical) else {
            result.push_str(&format!("<!-- @include {} not found -->\n", path_str));
            continue;
        };
        let include_dir = canonical.parent().unwrap_or(base_dir).to_path_buf();
        visited.insert(canonical);
        result.push_str(&expand_includes(&included, &include_dir, root, visited, depth + 1));
        result.push('\n');
    }
    result
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// The instruction files present in `dir`, in load order.
fn files_in_dir(dir: &Path) -> Vec<PathBuf> {
    let override_file = dir.join(OVERRIDE_FILE_NAME);
    if override_file.is_file() {
        return vec![override_file];
    }
    INSTRUCTION_FILE_PATHS
        .iter()
        .map(|components| components.iter().fold(dir.to_path_buf(), |path, c| path.join(c)))
        .filter(|path| path.is_file())
        .collect()
}

/// Load the instruction files for a session in `cwd`. `global_dirs` are the
/// user-level directories, in load order (normally `Settings::config_dir()`
/// then `~/.claude`, see [`default_global_dirs`]).
///
/// Returns the files in load order. Once the combined content reaches
/// [`INSTRUCTION_BUDGET_BYTES`], the file that crosses it is truncated with a
/// note and later files carry only a note naming them.
pub fn load_instruction_files(cwd: &Path, global_dirs: &[PathBuf]) -> Vec<InstructionFile> {
    let project_dirs = crate::skill_discovery::project_search_dirs(cwd);
    let project_root = project_dirs.last().map_or(cwd, PathBuf::as_path);

    // (file, root its @include lines are confined to)
    let mut sources: Vec<(PathBuf, &Path)> = global_dirs
        .iter()
        .flat_map(|dir| files_in_dir(dir).into_iter().map(move |file| (file, dir.as_path())))
        .collect();
    for dir in project_dirs.iter().rev() {
        sources.extend(files_in_dir(dir).into_iter().map(|file| (file, project_root)));
    }

    let mut files: Vec<InstructionFile> = Vec::new();
    let mut seen_paths: HashSet<PathBuf> = HashSet::new();
    let mut seen_contents: HashSet<String> = HashSet::new();
    let mut remaining = INSTRUCTION_BUDGET_BYTES;
    for (path, root) in sources {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
        if !seen_paths.insert(canonical.clone()) {
            continue;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(err) => {
                tracing::debug!(path = %path.display(), error = %err, "instruction file unreadable");
                continue;
            }
        };
        let base_dir = canonical.parent().unwrap_or(root).to_path_buf();
        let mut visited = HashSet::from([canonical]);
        let expanded = expand_includes(strip_frontmatter(&raw), &base_dir, root, &mut visited, 0);
        let content = expanded.trim();
        if content.is_empty() || !seen_contents.insert(content.to_string()) {
            continue;
        }

        let content = if remaining == 0 {
            format!(
                "[Not loaded: the {} KiB instruction budget is used up. Read {} directly if it is relevant.]",
                INSTRUCTION_BUDGET_BYTES / 1024,
                path.display()
            )
        } else if content.len() <= remaining {
            remaining -= content.len();
            content.to_string()
        } else {
            let mut cut = remaining;
            while !content.is_char_boundary(cut) {
                cut -= 1;
            }
            remaining = 0;
            format!(
                "{}\n\n[Truncated: instruction files are limited to {} KiB in total. Read {} for the rest.]",
                &content[..cut],
                INSTRUCTION_BUDGET_BYTES / 1024,
                path.display()
            )
        };
        files.push(InstructionFile { path, content });
    }
    files
}

/// The user-level instruction directories: `<claurst home>`, then
/// `~/.claude` (Claude Code's global `CLAUDE.md`).
pub fn default_global_dirs() -> Vec<PathBuf> {
    let mut global = vec![crate::config::Settings::config_dir()];
    if let Some(home) = dirs::home_dir() {
        global.push(home.join(".claude"));
    }
    global
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        (tmp, repo)
    }

    fn contents(files: &[InstructionFile]) -> Vec<&str> {
        files.iter().map(|f| f.content.as_str()).collect()
    }

    #[test]
    fn strip_frontmatter_basic() {
        assert_eq!(strip_frontmatter("---\npriority: 10\n---\nHello world"), "Hello world");
        assert_eq!(strip_frontmatter("No frontmatter here"), "No frontmatter here");
    }

    #[test]
    fn loads_global_then_root_to_cwd_agents_before_claude() {
        let (tmp, repo) = git_repo();
        let global = tmp.path().join("global");
        let pkg = repo.join("pkg");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(global.join("AGENTS.md"), "global").unwrap();
        std::fs::write(repo.join("AGENTS.md"), "root agents").unwrap();
        std::fs::write(repo.join("CLAUDE.md"), "root claude").unwrap();
        std::fs::write(pkg.join("CLAUDE.md"), "pkg claude").unwrap();

        let files = load_instruction_files(&pkg, std::slice::from_ref(&global));
        assert_eq!(contents(&files), ["global", "root agents", "root claude", "pkg claude"]);
        assert_eq!(files[0].path, global.join("AGENTS.md"));
        assert_eq!(files[3].path, pkg.join("CLAUDE.md"));
    }

    #[test]
    fn dot_claude_and_dot_claurst_files_follow_agents_and_claude() {
        let (tmp, repo) = git_repo();
        let global = tmp.path().join("global");
        let claude_home = tmp.path().join("claude-home");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&claude_home).unwrap();
        std::fs::create_dir_all(repo.join(".claude")).unwrap();
        std::fs::create_dir_all(repo.join(".claurst")).unwrap();
        std::fs::write(global.join("AGENTS.md"), "claurst global").unwrap();
        std::fs::write(claude_home.join("CLAUDE.md"), "claude global").unwrap();
        std::fs::write(repo.join(".claurst").join("AGENTS.md"), "memories").unwrap();
        std::fs::write(repo.join(".claude").join("CLAUDE.md"), "dot claude").unwrap();
        std::fs::write(repo.join("CLAUDE.md"), "claude").unwrap();
        std::fs::write(repo.join("AGENTS.md"), "agents").unwrap();

        let files = load_instruction_files(&repo, &[global, claude_home]);
        assert_eq!(
            contents(&files),
            ["claurst global", "claude global", "agents", "claude", "dot claude", "memories"]
        );
        assert_eq!(files[4].path, repo.join(".claude").join("CLAUDE.md"));
        assert_eq!(files[5].path, repo.join(".claurst").join("AGENTS.md"));

        // The override still replaces every file in its directory.
        std::fs::write(repo.join("AGENTS.override.md"), "override").unwrap();
        let files = load_instruction_files(&repo, &[]);
        assert_eq!(contents(&files), ["override"]);
    }

    #[test]
    fn override_replaces_the_directory_files() {
        let (_tmp, repo) = git_repo();
        std::fs::write(repo.join("AGENTS.md"), "agents").unwrap();
        std::fs::write(repo.join("CLAUDE.md"), "claude").unwrap();
        std::fs::write(repo.join("AGENTS.override.md"), "override").unwrap();

        let files = load_instruction_files(&repo, &[]);
        assert_eq!(contents(&files), ["override"]);
    }

    #[test]
    fn stops_at_git_root() {
        let (tmp, repo) = git_repo();
        // Above the git root: never loaded.
        std::fs::write(tmp.path().join("AGENTS.md"), "outside").unwrap();
        std::fs::write(repo.join("AGENTS.md"), "inside").unwrap();

        let files = load_instruction_files(&repo, &[]);
        assert_eq!(contents(&files), ["inside"]);
    }

    #[test]
    fn without_git_only_cwd_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "parent").unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "cwd").unwrap();

        let files = load_instruction_files(&cwd, &[]);
        assert_eq!(contents(&files), ["cwd"]);
    }

    #[test]
    fn duplicate_claude_md_is_skipped() {
        let (_tmp, repo) = git_repo();
        std::fs::write(repo.join("AGENTS.md"), "same rules").unwrap();
        std::fs::write(repo.join("CLAUDE.md"), "---\nx: y\n---\nsame rules\n").unwrap();

        let files = load_instruction_files(&repo, &[]);
        assert_eq!(contents(&files), ["same rules"]);
    }

    #[test]
    fn include_expands_inside_root_and_refuses_outside() {
        let (tmp, repo) = git_repo();
        std::fs::create_dir_all(repo.join("docs")).unwrap();
        std::fs::write(repo.join("docs").join("style.md"), "use tabs").unwrap();
        std::fs::write(tmp.path().join("secret.md"), "SECRET").unwrap();
        std::fs::write(
            repo.join("AGENTS.md"),
            "head\n@include docs/style.md\n@include ../secret.md\n",
        )
        .unwrap();

        let files = load_instruction_files(&repo, &[]);
        assert_eq!(files.len(), 1);
        assert!(files[0].content.contains("head\nuse tabs"));
        assert!(!files[0].content.contains("SECRET"));
        assert!(files[0].content.contains("skipped: outside"));
    }

    #[test]
    fn include_circular_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.md"), "@include b.md\n").unwrap();
        std::fs::write(tmp.path().join("b.md"), "@include a.md\ncontent\n").unwrap();
        let result = expand_includes("@include a.md\n", tmp.path(), tmp.path(), &mut HashSet::new(), 0);
        assert!(result.contains("circular"));
        assert!(result.contains("content"));
    }

    #[test]
    fn budget_truncates_then_names_skipped_files() {
        let (_tmp, repo) = git_repo();
        let pkg = repo.join("pkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(repo.join("AGENTS.md"), "a".repeat(INSTRUCTION_BUDGET_BYTES - 10)).unwrap();
        std::fs::write(repo.join("CLAUDE.md"), "b".repeat(100)).unwrap();
        std::fs::write(pkg.join("AGENTS.md"), "c".repeat(100)).unwrap();

        let files = load_instruction_files(&pkg, &[]);
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].content.len(), INSTRUCTION_BUDGET_BYTES - 10);
        assert!(files[1].content.starts_with(&"b".repeat(10)));
        assert!(!files[1].content.starts_with(&"b".repeat(11)));
        assert!(files[1].content.contains("[Truncated: instruction files are limited to 32 KiB"));
        assert!(files[2].content.starts_with("[Not loaded"));
        assert!(files[2].content.contains(&pkg.join("AGENTS.md").display().to_string()));
    }
}
