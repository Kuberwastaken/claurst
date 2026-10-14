//! Skill discovery: load custom prompt-template skills from markdown files
//! on disk and (optionally) from git URLs, and assemble them with the bundled
//! skills into the session [`SkillRegistry`].
//!
//! Search priority (first match wins for a given skill name):
//!   1. Project `.claurst/skills/`, `.agents/skills/`, `.claude/skills/` in
//!      each directory from `cwd` up to the git root (see
//!      [`project_search_dirs`]); nearer directories win
//!   2. Global `<claurst home>/skills/`, `~/.agents/skills/`, `~/.claude/skills/`
//!   3. Configured extra paths from `SkillsConfig.paths`
//!   4. Git-URL repos from `SkillsConfig.urls` (cloned once, then cached)
//!
//! Bundled skills (`crate::bundled_skills`) come last: a discovered skill with
//! the same name replaces the bundled one.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A discovered skill loaded from a markdown file.
#[derive(Debug, Clone)]
pub struct DiscoveredSkill {
    /// Skill name (from `name:` frontmatter or file stem).
    pub name: String,
    /// One-line description (from `description:` frontmatter or default).
    pub description: String,
    /// The prompt body after stripping frontmatter.
    pub template: String,
    /// Absolute path to the source `.md` file.
    pub source_path: PathBuf,
}

// ---------------------------------------------------------------------------
// Frontmatter parsing
// ---------------------------------------------------------------------------

/// Parse a skill markdown file.
///
/// Expects optional YAML frontmatter delimited by `---`.
/// Returns `None` when the file is empty after trimming.
pub fn parse_skill_file(content: &str, path: &Path) -> Option<DiscoveredSkill> {
    let content = content.trim();
    if content.is_empty() {
        return None;
    }

    let (name, description, template) = if let Some(after_open) = content.strip_prefix("---") {
        // Accept both `\n---` and `\r\n---` as closing delimiter.
        if let Some(close_pos) = after_open.find("\n---") {
            let frontmatter = &after_open[..close_pos];
            let rest = after_open[close_pos + 4..].trim_start_matches(['\r', '\n']);

            let mut name: Option<String> = None;
            let mut description: Option<String> = None;

            for line in frontmatter.lines() {
                let line = line.trim();
                if let Some(v) = line.strip_prefix("name:") {
                    name = Some(v.trim().trim_matches('"').trim_matches('\'').to_string());
                } else if let Some(v) = line.strip_prefix("description:") {
                    description = Some(v.trim().trim_matches('"').trim_matches('\'').to_string());
                }
            }

            (name, description, rest.to_string())
        } else {
            // Malformed frontmatter — treat entire content as template.
            (None, None, content.to_string())
        }
    } else {
        (None, None, content.to_string())
    };

    let name = name.unwrap_or_else(|| {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unnamed");
        // When the file is named `SKILL.md` (directory-based skill), use the
        // parent directory's name as the skill name.
        if stem.eq_ignore_ascii_case("skill") {
            path.parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or(stem)
                .to_string()
        } else {
            stem.to_string()
        }
    });
    let description = description.unwrap_or_else(|| "Custom skill".to_string());

    if template.is_empty() && name.is_empty() {
        return None;
    }

    Some(DiscoveredSkill {
        name,
        description,
        template,
        source_path: path.to_path_buf(),
    })
}

// ---------------------------------------------------------------------------
// Directory scanning
// ---------------------------------------------------------------------------

/// Scan a single directory for skill files:
/// - Flat `*.md` files directly in `dir`
/// - Subdirectories containing `SKILL.md` (e.g. `dir/<skill-name>/SKILL.md`)
/// - `dir` itself if it directly contains `SKILL.md`
///
/// Collects all matches rather than returning early, so a directory with a
/// root `SKILL.md` plus sibling flat `.md` files keeps the siblings.
fn scan_dir(dir: &Path) -> Vec<DiscoveredSkill> {
    let mut skills = Vec::new();
    if !dir.is_dir() {
        return skills;
    }

    // If `dir` directly contains a SKILL.md file, parse it as a single skill.
    // Both casings are tried, but only the first hit is used: on
    // case-insensitive filesystems (macOS, Windows) `SKILL.md` and `skill.md`
    // resolve to the same file, so taking both would double-count.
    for skill_file in ["SKILL.md", "skill.md"] {
        let candidate = dir.join(skill_file);
        if candidate.is_file() {
            if let Ok(content) = std::fs::read_to_string(&candidate) {
                if let Some(skill) = parse_skill_file(&content, &candidate) {
                    skills.push(skill);
                }
            }
            break;
        }
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(err) => {
            tracing::debug!(dir = %dir.display(), error = %err, "skill_discovery: read_dir failed");
            return skills;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let file_name = entry.file_name();
        let file_name_str = file_name.to_string_lossy();

        // Skip hidden files/directories (e.g. .git, .DS_Store).
        if file_name_str.starts_with('.') {
            continue;
        }

        if path.is_file() {
            // Skip SKILL.md / skill.md — already handled by the dedicated
            // check above, so we don't double-count them.
            if file_name_str.eq_ignore_ascii_case("skill.md") {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                match std::fs::read_to_string(&path) {
                    Ok(content) => {
                        if let Some(skill) = parse_skill_file(&content, &path) {
                            skills.push(skill);
                        }
                    }
                    Err(err) => {
                        tracing::debug!(path = %path.display(), error = %err, "skill_discovery: read failed");
                    }
                }
            }
        } else if path.is_dir() {
            // Check subdirectories for SKILL.md / skill.md.
            for skill_file in ["SKILL.md", "skill.md"] {
                let candidate = path.join(skill_file);
                if candidate.is_file() {
                    match std::fs::read_to_string(&candidate) {
                        Ok(content) => {
                            if let Some(skill) = parse_skill_file(&content, &candidate) {
                                skills.push(skill);
                            }
                        }
                        Err(err) => {
                            tracing::debug!(path = %candidate.display(), error = %err, "skill_discovery: read failed");
                        }
                    }
                    break;
                }
            }
        }
    }

    skills
}

/// Resolve a configured path string, expanding `~` to the user's home
/// directory. `~` followed by any platform separator (`/`, plus `\` on
/// Windows) is expanded; when the home directory is unknown the path is left
/// untouched. `~user` paths (e.g. `~alice/skills`) are left unexpanded rather
/// than silently resolving to the wrong home.
fn resolve_path(path_str: &str, cwd: &Path) -> PathBuf {
    let trimmed = path_str.trim();
    if trimmed == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from(trimmed))
    } else if let Some(rest) = trimmed
        .strip_prefix('~')
        .and_then(|r| r.strip_prefix(std::path::is_separator))
    {
        dirs::home_dir().map(|h| h.join(rest)).unwrap_or_else(|| PathBuf::from(trimmed))
    } else if trimmed.starts_with('~') {
        // `~user/...` — we can't resolve another user's home reliably.
        // Leave the path unexpanded rather than guessing wrong.
        PathBuf::from(trimmed)
    } else {
        let p = Path::new(trimmed);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        }
    }
}

// ---------------------------------------------------------------------------
// Top-level discovery
// ---------------------------------------------------------------------------

/// Maximum number of directories [`project_search_dirs`] climbs from `cwd`
/// while looking for the git root.
const MAX_PROJECT_DEPTH: usize = 16;

/// Directories searched for project-scoped files, innermost (`cwd`) first.
///
/// Climbs from `cwd` to the nearest ancestor containing `.git` (inclusive).
/// When no `.git` is found within [`MAX_PROJECT_DEPTH`] levels, only `cwd`
/// itself is returned, so a stray file in `$HOME` or `/` is never picked up.
pub fn project_search_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if dirs.len() >= MAX_PROJECT_DEPTH {
            break;
        }
        dirs.push(d.to_path_buf());
        if d.join(".git").exists() {
            return dirs;
        }
        dir = d.parent();
    }
    vec![cwd.to_path_buf()]
}

/// Discover all skills from all configured sources.
///
/// Returns a `HashMap` of `skill_name → DiscoveredSkill` (first match wins;
/// duplicates from lower-priority sources are warned via `tracing::warn`).
pub fn discover_skills(
    cwd: &Path,
    config_skills: &crate::config::SkillsConfig,
) -> HashMap<String, DiscoveredSkill> {
    let mut all: HashMap<String, DiscoveredSkill> = HashMap::new();
    let mut warn_duplicates: Vec<String> = Vec::new();
    // A directory reachable twice (e.g. cwd is `$HOME`, so `.agents/skills`
    // is both a project and a global dir) is scanned once.
    let mut scanned: HashSet<PathBuf> = HashSet::new();

    let mut add_dir = |dir: &Path| {
        let key = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        if !scanned.insert(key) {
            return;
        }
        for skill in scan_dir(dir) {
            if let Some(existing) = all.get(&skill.name) {
                warn_duplicates.push(format!(
                    "Duplicate skill '{}' found at {} (keeping {})",
                    skill.name,
                    skill.source_path.display(),
                    existing.source_path.display()
                ));
            } else {
                all.insert(skill.name.clone(), skill);
            }
        }
    };

    // ---- 1. Project skills: cwd up to the git root --------------------------
    for dir in project_search_dirs(cwd) {
        for root in [".claurst", ".agents", ".claude"] {
            add_dir(&dir.join(root).join("skills"));
        }
    }

    // ---- 2. Global skills ---------------------------------------------------
    add_dir(&crate::config::Settings::config_dir().join("skills"));
    if let Some(home) = dirs::home_dir() {
        add_dir(&home.join(".agents").join("skills"));
        add_dir(&home.join(".claude").join("skills"));
    }

    // ---- 3. Configured extra paths (with ~ expansion) -----------------------
    for path_str in &config_skills.paths {
        add_dir(&resolve_path(path_str, cwd));
    }

    // ---- 4. Git URL skills (cached) -----------------------------------------
    for url in &config_skills.urls {
        if let Some(repo_dir) = fetch_git_skills(url) {
            add_dir(&repo_dir);
            add_dir(&repo_dir.join("skills"));
        }
    }

    // Emit warnings for any duplicate skill names encountered.
    for w in &warn_duplicates {
        tracing::warn!("{}", w);
    }

    all
}

// ---------------------------------------------------------------------------
// Session skill registry
// ---------------------------------------------------------------------------

/// One skill in the [`SkillRegistry`].
#[derive(Debug, Clone)]
pub enum RegisteredSkill {
    /// Compiled into the binary (`crate::bundled_skills`).
    Bundled(&'static crate::bundled_skills::BundledSkill),
    /// Loaded from a markdown file on disk or from a cloned skills repo.
    Discovered(DiscoveredSkill),
}

impl RegisteredSkill {
    pub fn name(&self) -> &str {
        match self {
            Self::Bundled(b) => b.name,
            Self::Discovered(d) => &d.name,
        }
    }

    pub fn description(&self) -> &str {
        match self {
            Self::Bundled(b) => b.description,
            Self::Discovered(d) => &d.description,
        }
    }

    /// The markdown file the skill was loaded from; `None` for bundled skills.
    pub fn source_path(&self) -> Option<&Path> {
        match self {
            Self::Bundled(_) => None,
            Self::Discovered(d) => Some(&d.source_path),
        }
    }

    /// The directory relative paths inside the skill resolve against: the
    /// folder holding the skill file. `None` for bundled skills.
    pub fn base_dir(&self) -> Option<&Path> {
        self.source_path().and_then(Path::parent)
    }

    /// The skill body with `$ARGUMENTS` substituted.
    pub fn expand(&self, args: &str) -> String {
        match self {
            Self::Bundled(b) => crate::bundled_skills::expand_prompt(b, args),
            Self::Discovered(d) => d.template.replace("$ARGUMENTS", args),
        }
    }
}

/// Every skill available to a session, sorted by name.
///
/// The single source for the Skill tool, the `/skills` listing, skill slash
/// commands and the `<available_skills>` block in the system prompt. Built
/// from [`discover_skills`] plus the user-invocable bundled skills; a
/// discovered skill replaces a bundled one with the same name.
#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: Vec<RegisteredSkill>,
}

impl SkillRegistry {
    /// Discover skills for `cwd` and merge in the bundled ones.
    ///
    /// May `git clone` configured skill URLs: call it off the UI thread.
    pub fn discover(cwd: &Path, config_skills: &crate::config::SkillsConfig) -> Self {
        Self::from_discovered(discover_skills(cwd, config_skills))
    }

    fn from_discovered(discovered: HashMap<String, DiscoveredSkill>) -> Self {
        let bundled = crate::bundled_skills::BUNDLED_SKILLS
            .iter()
            .filter(|b| b.user_invocable && !discovered.contains_key(b.name))
            .map(RegisteredSkill::Bundled)
            .collect::<Vec<_>>();
        let mut skills: Vec<RegisteredSkill> = discovered
            .into_values()
            .map(RegisteredSkill::Discovered)
            .chain(bundled)
            .collect();
        skills.sort_by(|a, b| a.name().cmp(b.name()));
        Self { skills }
    }

    /// All skills, sorted by name.
    pub fn skills(&self) -> &[RegisteredSkill] {
        &self.skills
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Look a skill up by exact name, or by a bundled skill's alias.
    pub fn get(&self, name: &str) -> Option<&RegisteredSkill> {
        self.skills.iter().find(|s| s.name() == name).or_else(|| {
            self.skills.iter().find(|s| match s {
                RegisteredSkill::Bundled(b) => b.aliases.contains(&name),
                RegisteredSkill::Discovered(_) => false,
            })
        })
    }

    /// Name and description of every skill, in registry order, for the
    /// `<available_skills>` block in the system prompt.
    pub fn listings(&self) -> Vec<crate::system_prompt::SkillListing> {
        self.skills
            .iter()
            .map(|skill| crate::system_prompt::SkillListing {
                name: skill.name().to_string(),
                description: skill.description().to_string(),
            })
            .collect()
    }

    /// The skills loaded from disk or a skills repo (not bundled).
    pub fn discovered(&self) -> impl Iterator<Item = &DiscoveredSkill> {
        self.skills.iter().filter_map(|s| match s {
            RegisteredSkill::Discovered(d) => Some(d),
            RegisteredSkill::Bundled(_) => None,
        })
    }
}

// ---------------------------------------------------------------------------
// Git URL support
// ---------------------------------------------------------------------------

/// Clone or reuse a cached git repo and return its local directory. Skills
/// are read from the repo root and its optional `skills/` subdirectory.
///
/// Cache location: `<system-cache>/claurst/skills/<repo-name>/`
/// On first access the repo is cloned with `--depth=1`.
/// Subsequent calls use the already-cloned cache directory as-is.
fn fetch_git_skills(url: &str) -> Option<PathBuf> {
    let cache_dir = dirs::cache_dir()?.join("claurst").join("skills");

    // Use the last path segment of the URL as the local directory name.
    let repo_name = url
        .split('/')
        .next_back()?
        .trim_end_matches(".git");

    if repo_name.is_empty() {
        tracing::warn!(url, "skill_discovery: cannot derive repo name from git URL");
        return None;
    }

    let repo_dir = cache_dir.join(repo_name);

    if !repo_dir.exists() {
        tracing::info!(url, dest = %repo_dir.display(), "skill_discovery: cloning skills repo");

        // Ensure the parent cache directory exists.
        if let Err(err) = std::fs::create_dir_all(&cache_dir) {
            tracing::warn!(
                dir = %cache_dir.display(),
                error = %err,
                "skill_discovery: could not create cache dir"
            );
            return None;
        }

        let repo_dir_str = repo_dir.to_str()?;
        // Discovery can run while the TUI owns the terminal: keep git off the
        // tty entirely and fail instead of prompting for credentials.
        let status = std::process::Command::new("git")
            .args(["clone", "--depth=1", url, repo_dir_str])
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        match status {
            Ok(s) if s.success() => {
                tracing::info!(url, "skill_discovery: clone succeeded");
            }
            Ok(s) => {
                tracing::warn!(url, exit_code = ?s.code(), "skill_discovery: git clone failed");
                return None;
            }
            Err(err) => {
                tracing::warn!(url, error = %err, "skill_discovery: could not spawn git");
                return None;
            }
        }
    }

    Some(repo_dir)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as IoWrite;

    fn make_temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn write_file(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    // ---- parse_skill_file ---------------------------------------------------

    #[test]
    fn test_parse_with_frontmatter() {
        let content = "---\nname: review\ndescription: Review code changes\n---\n\nPlease review $ARGUMENTS";
        let path = PathBuf::from("review.md");
        let skill = parse_skill_file(content, &path).unwrap();
        assert_eq!(skill.name, "review");
        assert_eq!(skill.description, "Review code changes");
        assert!(skill.template.contains("$ARGUMENTS"));
    }

    #[test]
    fn test_parse_no_frontmatter_uses_stem() {
        let content = "Do something useful.";
        let path = PathBuf::from("my-skill.md");
        let skill = parse_skill_file(content, &path).unwrap();
        assert_eq!(skill.name, "my-skill");
        assert_eq!(skill.description, "Custom skill");
        assert_eq!(skill.template, "Do something useful.");
    }

    #[test]
    fn test_parse_missing_name_uses_stem() {
        let content = "---\ndescription: No name field\n---\n\nBody text.";
        let path = PathBuf::from("fallback.md");
        let skill = parse_skill_file(content, &path).unwrap();
        assert_eq!(skill.name, "fallback");
        assert_eq!(skill.description, "No name field");
    }

    #[test]
    fn test_parse_empty_returns_none() {
        let skill = parse_skill_file("   ", &PathBuf::from("empty.md"));
        assert!(skill.is_none());
    }

    #[test]
    fn test_parse_quoted_frontmatter_values() {
        let content = "---\nname: \"quoted name\"\ndescription: 'single quoted'\n---\nBody.";
        let skill = parse_skill_file(content, &PathBuf::from("x.md")).unwrap();
        assert_eq!(skill.name, "quoted name");
        assert_eq!(skill.description, "single quoted");
    }

    // ---- scan_dir -----------------------------------------------------------

    #[test]
    fn test_scan_dir_finds_skills() {
        let tmp = make_temp_dir();
        write_file(tmp.path(), "review.md", "---\nname: review\n---\nReview $ARGUMENTS");
        write_file(tmp.path(), "debug.md", "Debug help.");
        write_file(tmp.path(), "not-md.txt", "ignored");

        let skills = scan_dir(tmp.path());
        assert_eq!(skills.len(), 2);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"review"));
        assert!(names.contains(&"debug"));
    }

    #[test]
    fn test_scan_dir_nonexistent_returns_empty() {
        let skills = scan_dir(Path::new("/nonexistent/path/xyz"));
        assert!(skills.is_empty());
    }

    // ---- discover_skills ----------------------------------------------------

    #[test]
    fn test_discover_from_project_dir() {
        let tmp = make_temp_dir();
        let skills_dir = tmp.path().join(".claurst").join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        write_file(&skills_dir, "myskill.md", "---\nname: myskill\ndescription: Test\n---\nDo it.");

        let config = crate::config::SkillsConfig::default();
        let discovered = discover_skills(tmp.path(), &config);
        assert!(discovered.contains_key("myskill"));
        assert_eq!(discovered["myskill"].description, "Test");
    }

    #[test]
    fn test_discover_extra_paths() {
        let tmp = make_temp_dir();
        let extra = make_temp_dir();
        write_file(extra.path(), "extra.md", "---\nname: extra\n---\nExtra skill.");

        let config = crate::config::SkillsConfig {
            paths: vec![extra.path().to_str().unwrap().to_string()],
            urls: vec![],
        };
        let discovered = discover_skills(tmp.path(), &config);
        assert!(discovered.contains_key("extra"));
    }

    #[test]
    fn test_discover_deduplicates_first_wins() {
        let tmp = make_temp_dir();
        let proj_skills = tmp.path().join(".claurst").join("skills");
        std::fs::create_dir_all(&proj_skills).unwrap();
        write_file(&proj_skills, "dup.md", "---\nname: dup\ndescription: project\n---\nProject.");

        let extra = make_temp_dir();
        write_file(extra.path(), "dup.md", "---\nname: dup\ndescription: extra\n---\nExtra.");

        let config = crate::config::SkillsConfig {
            paths: vec![extra.path().to_str().unwrap().to_string()],
            urls: vec![],
        };
        let discovered = discover_skills(tmp.path(), &config);
        // Project-level wins over extra path.
        assert_eq!(discovered["dup"].description, "project");
    }

    // ---- directory-based skills (SKILL.md in subdirs) -----------------------

    #[test]
    fn test_parse_skill_md_in_subdir_uses_parent_dir_name() {
        let content = "Use clean code principles.";
        let path = PathBuf::from("/home/user/.agents/skills/clean-code/SKILL.md");
        let skill = parse_skill_file(content, &path).unwrap();
        assert_eq!(skill.name, "clean-code");
        assert_eq!(skill.description, "Custom skill");
        assert_eq!(skill.template, "Use clean code principles.");
    }

    #[test]
    fn test_scan_dir_finds_subdirectories_with_skill_md() {
        let tmp = make_temp_dir();
        // Flat file
        write_file(tmp.path(), "flat.md", "---\nname: flat\n---\nFlat template.");
        // Subdirectory with SKILL.md
        write_file(
            tmp.path(),
            "brainstorming/SKILL.md",
            "---\ndescription: Brainstorm ideas\n---\nBrainstorm $ARGUMENTS",
        );
        // Subdirectory with lowercase skill.md
        write_file(
            tmp.path(),
            "git-master/skill.md",
            "---\nname: git-guru\ndescription: Git helpers\n---\nGit commands",
        );
        // Ignored non-skill dir
        write_file(tmp.path(), "other_folder/readme.txt", "not a skill");

        let skills = scan_dir(tmp.path());
        assert_eq!(skills.len(), 3);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"flat"));
        assert!(names.contains(&"brainstorming"));
        assert!(names.contains(&"git-guru"));
    }

    #[test]
    fn test_scan_dir_direct_skill_folder() {
        let tmp = make_temp_dir();
        let skill_dir = tmp.path().join("my-custom-skill");
        write_file(
            &skill_dir,
            "SKILL.md",
            "---\ndescription: Direct folder\n---\nDo work",
        );

        let skills = scan_dir(&skill_dir);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "my-custom-skill");
        assert_eq!(skills[0].description, "Direct folder");
    }

    // ---- resolve_path -------------------------------------------------------

    #[test]
    fn test_resolve_path() {
        let cwd = PathBuf::from("/workspace/project");
        // Absolute path
        assert_eq!(
            resolve_path("/Users/test/.agents/skills", &cwd),
            PathBuf::from("/Users/test/.agents/skills")
        );
        // Relative path
        assert_eq!(
            resolve_path("custom/skills", &cwd),
            PathBuf::from("/workspace/project/custom/skills")
        );
        // Tilde path
        if let Some(home) = dirs::home_dir() {
            assert_eq!(
                resolve_path("~/.agents/skills", &cwd),
                home.join(".agents/skills")
            );
            assert_eq!(resolve_path("~", &cwd), home);
        }
        // ~user path should NOT be expanded to $HOME
        let unexpanded = resolve_path("~alice/skills", &cwd);
        assert_eq!(unexpanded, PathBuf::from("~alice/skills"));
    }

    #[cfg(windows)]
    #[test]
    fn test_resolve_path_windows_backslash_tilde() {
        let cwd = PathBuf::from(r"C:\workspace\project");
        if let Some(home) = dirs::home_dir() {
            assert_eq!(
                resolve_path(r"~\.agents\skills", &cwd),
                home.join(".agents").join("skills")
            );
        }
        assert_eq!(resolve_path(r"~alice\skills", &cwd), PathBuf::from(r"~alice\skills"));
    }

    // ---- discover_skills with directory-based skills ------------------------

    #[test]
    fn test_discover_from_project_agents_skills_subdir() {
        let tmp = make_temp_dir();
        let skills_dir = tmp.path().join(".agents").join("skills").join("sub-skill");
        std::fs::create_dir_all(&skills_dir).unwrap();
        write_file(
            &skills_dir,
            "SKILL.md",
            "---\ndescription: Sub agent skill\n---\nSub agent prompt.",
        );

        let config = crate::config::SkillsConfig::default();
        let discovered = discover_skills(tmp.path(), &config);
        assert!(discovered.contains_key("sub-skill"));
        assert_eq!(discovered["sub-skill"].description, "Sub agent skill");
    }

    // ---- project_search_dirs ------------------------------------------------

    #[test]
    fn project_search_dirs_stop_at_git_root() {
        let tmp = make_temp_dir();
        let repo = tmp.path().join("repo");
        let nested = repo.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        let dirs = project_search_dirs(&nested);
        assert_eq!(dirs, vec![nested.clone(), repo.join("a"), repo.clone()]);
        assert!(!dirs.contains(&tmp.path().to_path_buf()));
    }

    #[test]
    fn project_search_dirs_without_git_is_cwd_only() {
        let tmp = make_temp_dir();
        let nested = tmp.path().join("x").join("y");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(project_search_dirs(&nested), vec![nested]);
    }

    #[test]
    fn test_discover_project_roots_up_to_git_root_only() {
        let tmp = make_temp_dir();
        let repo = tmp.path().join("repo");
        let cwd = repo.join("pkg");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        write_file(
            &repo.join(".claude").join("skills").join("from-claude-446"),
            "SKILL.md",
            "---\ndescription: claude dir\n---\nBody.",
        );
        write_file(&cwd.join(".agents").join("skills"), "near-446.md", "Near.");
        // Above the git root: must not be discovered.
        write_file(&tmp.path().join(".claurst").join("skills"), "outside-446.md", "Outside.");

        let discovered = discover_skills(&cwd, &crate::config::SkillsConfig::default());
        assert_eq!(discovered["from-claude-446"].description, "claude dir");
        assert!(discovered.contains_key("near-446"));
        assert!(!discovered.contains_key("outside-446"));
    }

    // ---- SkillRegistry ------------------------------------------------------

    #[test]
    fn registry_merges_bundled_and_discovered_sorted() {
        // Built from fixtures directly so skills in the real global dirs of
        // the machine running the test cannot interfere.
        let tmp = make_temp_dir();
        let skills_dir = tmp.path().join("skills");
        write_file(&skills_dir.join("zeta"), "SKILL.md", "---\ndescription: Z\n---\nZ body $ARGUMENTS");
        // Same name as a bundled skill: the file replaces it.
        write_file(&skills_dir, "simplify.md", "---\nname: simplify\ndescription: mine\n---\nMine.");
        let discovered: HashMap<String, DiscoveredSkill> = scan_dir(&skills_dir)
            .into_iter()
            .map(|skill| (skill.name.clone(), skill))
            .collect();
        let registry = SkillRegistry::from_discovered(discovered);

        let names: Vec<&str> = registry.skills().iter().map(RegisteredSkill::name).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "registry must be sorted by name");
        assert_eq!(names.iter().filter(|n| **n == "simplify").count(), 1);

        let simplify = registry.get("simplify").unwrap();
        assert_eq!(simplify.description(), "mine");
        assert!(matches!(simplify, RegisteredSkill::Discovered(_)));

        let zeta = registry.get("zeta").unwrap();
        assert_eq!(zeta.base_dir(), Some(skills_dir.join("zeta").as_path()));
        assert_eq!(zeta.expand("now"), "Z body now");

        let debug = registry.get("debug").unwrap();
        assert!(matches!(debug, RegisteredSkill::Bundled(_)));
        assert!(debug.base_dir().is_none());
        // Bundled aliases resolve to the bundled skill.
        assert_eq!(registry.get("mem").map(RegisteredSkill::name), Some("remember"));

        let from_disk: Vec<&str> = registry.discovered().map(|d| d.name.as_str()).collect();
        assert!(from_disk.contains(&"zeta"));
        assert!(!from_disk.contains(&"debug"));
    }
}
