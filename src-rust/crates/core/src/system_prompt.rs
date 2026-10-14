//! Modular system prompt assembly with caching support.
//!
//! Mirrors the TypeScript `systemPromptSections.ts` / `prompts.ts` architecture:
//! cacheable (static) sections are placed before `SYSTEM_PROMPT_DYNAMIC_BOUNDARY`;
//! volatile, session-specific sections follow it.

use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Dynamic boundary marker
// ---------------------------------------------------------------------------

/// Marker that splits the cached vs dynamic parts of the system prompt.
/// Everything before this marker can be prompt-cached by the API.
/// Matches the TypeScript constant `SYSTEM_PROMPT_DYNAMIC_BOUNDARY`.
pub const SYSTEM_PROMPT_DYNAMIC_BOUNDARY: &str = "__SYSTEM_PROMPT_DYNAMIC_BOUNDARY__";

/// Split an assembled system prompt into `(stable, dynamic)` at
/// [`SYSTEM_PROMPT_DYNAMIC_BOUNDARY`]. The marker is dropped and the newlines
/// around it are trimmed. A prompt without the marker is entirely stable.
pub fn split_at_dynamic_boundary(prompt: &str) -> (&str, &str) {
    match prompt.split_once(SYSTEM_PROMPT_DYNAMIC_BOUNDARY) {
        Some((stable, dynamic)) => (stable.trim_end(), dynamic.trim_start()),
        None => (prompt, ""),
    }
}

// ---------------------------------------------------------------------------
// Section cache (mirrors bootstrap/state.ts systemPromptSectionCache)
// ---------------------------------------------------------------------------

fn section_cache() -> &'static Mutex<HashMap<String, Option<String>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Clear all cached system prompt sections (called on /clear and /compact).
pub fn clear_system_prompt_sections() {
    if let Ok(mut cache) = section_cache().lock() {
        cache.clear();
    }
}

/// A single named section of the system prompt.
#[derive(Debug, Clone)]
pub struct SystemPromptSection {
    /// Identifier used for cache lookups and invalidation.
    pub tag: &'static str,
    /// Computed content (None means the section is absent/disabled).
    pub content: Option<String>,
    /// If true the section is volatile and must not be prompt-cached.
    pub cache_break: bool,
}

impl SystemPromptSection {
    /// Create a memoizable (cacheable) section.
    pub fn cached(tag: &'static str, content: impl Into<String>) -> Self {
        Self { tag, content: Some(content.into()), cache_break: false }
    }

    /// Create a volatile section that re-evaluates every turn.
    /// Passing `None` for content means the section is absent this turn.
    pub fn uncached(tag: &'static str, content: Option<impl Into<String>>) -> Self {
        Self {
            tag,
            content: content.map(|c| c.into()),
            cache_break: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Output style
// ---------------------------------------------------------------------------

/// Output styles that affect the system prompt.
/// Serialised as lowercase strings to match settings.json.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum OutputStyle {
    #[default]
    Default,
    Explanatory,
    Learning,
    Concise,
    Formal,
    Casual,
}

impl OutputStyle {
    /// Returns the system-prompt suffix for this style, or `None` for Default.
    pub fn prompt_suffix(self) -> Option<&'static str> {
        match self {
            OutputStyle::Explanatory => Some(
                "When explaining code or concepts, be thorough and educational. \
                Include reasoning, alternatives considered, and potential pitfalls. \
                Err on the side of over-explaining.",
            ),
            OutputStyle::Learning => Some(
                "This user is learning. Explain concepts as you implement them. \
                Point out patterns, best practices, and why you made each decision. \
                Use analogies when helpful.",
            ),
            OutputStyle::Concise => Some(
                "Be maximally concise. Skip preamble, summaries, and filler. \
                Lead with the answer. One sentence is better than three.",
            ),
            OutputStyle::Formal => Some(
                "Maintain a formal, professional tone. Use precise technical language.",
            ),
            OutputStyle::Casual => Some("Use a casual, conversational tone."),
            OutputStyle::Default => None,
        }
    }

    /// Parse from a string (case-insensitive).
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "explanatory" => Self::Explanatory,
            "learning" => Self::Learning,
            "concise" => Self::Concise,
            "formal" => Self::Formal,
            "casual" => Self::Casual,
            _ => Self::Default,
        }
    }
}

// ---------------------------------------------------------------------------
// System prompt prefix variants
// ---------------------------------------------------------------------------

/// Which entrypoint context Claurst is running in.
/// Determines the opening attribution line of the system prompt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SystemPromptPrefix {
    /// Standard interactive CLI session.
    Cli,
    /// Running as a sub-agent spawned by the Claude Agent SDK.
    Sdk,
    /// The CLI preset running within the Agent SDK
    /// (non-interactive + append_system_prompt set).
    SdkPreset,
    /// Running on Vertex AI.
    Vertex,
    /// Running on AWS Bedrock.
    Bedrock,
    /// Remote / headless CCR session.
    Remote,
}

impl SystemPromptPrefix {
    /// Detect from environment variables, mirroring `getCLISyspromptPrefix`.
    pub fn detect(is_non_interactive: bool, has_append_system_prompt: bool) -> Self {
        // Vertex: always uses the default "Claurst" prefix.
        if std::env::var("ANTHROPIC_VERTEX_PROJECT_ID").is_ok()
            || std::env::var("CLOUD_ML_PROJECT_ID").is_ok()
        {
            return Self::Vertex;
        }

        if std::env::var("AWS_BEDROCK_MODEL_ID").is_ok() {
            return Self::Bedrock;
        }

        if std::env::var("CLAURST_REMOTE").is_ok() {
            return Self::Remote;
        }

        // Non-interactive mode maps to SDK variants (matches TS getCLISyspromptPrefix).
        if is_non_interactive {
            if has_append_system_prompt {
                return Self::SdkPreset;
            }
            return Self::Sdk;
        }

        Self::Cli
    }

    /// The opening attribution string for this prefix variant. It names no
    /// model vendor: the same prompt is sent to every provider.
    pub fn attribution_text(self) -> &'static str {
        match self {
            Self::Cli | Self::Vertex | Self::Bedrock | Self::Remote => {
                "You are Claurst, a coding agent that runs in the user's terminal. \
                You help by reading files, running commands, editing code, and writing new files."
            }
            Self::SdkPreset => {
                "You are Claurst, a coding agent running within the Claude Agent SDK. \
                You help by reading files, running commands, editing code, and writing new files."
            }
            Self::Sdk => {
                "You are a Claude agent, built on Anthropic's Claude Agent SDK."
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Platform
// ---------------------------------------------------------------------------

/// The platform the session runs on. It selects the single platform block in
/// the stable section, so the model gets one consistent shell-syntax rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptPlatform {
    Linux,
    MacOs,
    Windows,
}

impl PromptPlatform {
    /// The platform this binary was built for.
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Linux
        }
    }
}

// ---------------------------------------------------------------------------
// Build options
// ---------------------------------------------------------------------------

/// All options controlling what goes into the assembled system prompt.
#[derive(Debug, Clone, Default)]
pub struct SystemPromptOptions {
    /// Override auto-detected prefix.
    pub prefix: Option<SystemPromptPrefix>,
    /// Whether the session is non-interactive (SDK / pipe mode).
    pub is_non_interactive: bool,
    /// Whether --append-system-prompt is set (affects prefix detection).
    pub has_append_system_prompt: bool,
    /// Output style to inject.
    pub output_style: OutputStyle,
    /// Optional custom output-style prompt loaded from disk or plugins.
    /// When present, this overrides the built-in enum-derived suffix.
    pub custom_output_style_prompt: Option<String>,
    /// Absolute path to the working directory (injected as dynamic section).
    pub working_directory: Option<String>,
    /// Extra workspace roots (`/add-dir`), listed in `<environment_context>`
    /// after the working directory.
    pub additional_dirs: Vec<String>,
    /// Pre-built memory content from memdir (injected as dynamic section).
    pub memory_content: String,
    /// Session context gathered at startup (git status, IDE context).
    /// Injected in the dynamic section so it never invalidates the cached
    /// stable prefix.
    pub session_context: Option<String>,
    /// Custom system prompt (--system-prompt flag or settings).
    pub custom_system_prompt: Option<String>,
    /// Additional text appended after everything else (--append-system-prompt).
    pub append_system_prompt: Option<String>,
    /// If true and `custom_system_prompt` is set, the entire default prompt is
    /// replaced — only the custom text + dynamic boundary are emitted.
    pub replace_system_prompt: bool,
    /// Inject the coordinator-mode section.
    pub coordinator_mode: bool,
    /// Skip auto-injecting platform/shell env info (set true only in tests).
    pub skip_env_info: bool,
    /// Active goal addendum (injected in dynamic section when a goal is running).
    pub active_goal_addendum: Option<String>,
    /// Names of the tools actually enabled for this session (issue #233).
    ///
    /// When `Some`, the "Tool use guidelines" section only emits the
    /// per-tool guidance blocks for tools present in this list, so we don't
    /// pay the fixed prompt tax for guidance about tools that aren't loaded.
    /// When `None` (the default), the enabled set is treated as *unknown* and
    /// all per-tool guidance is emitted — preserving the previous behaviour
    /// for callers that don't yet thread the tool list through.
    pub enabled_tools: Option<Vec<String>>,
    /// Platform for the platform block; `None` means [`PromptPlatform::current`].
    pub platform: Option<PromptPlatform>,
    /// Current permission mode. It can change mid-session (/yolo, Shift+Tab),
    /// so its sentence goes in the dynamic section; `None` omits it.
    pub permission_mode: Option<crate::config::PermissionMode>,
}

// ---------------------------------------------------------------------------
// Main assembly function
// ---------------------------------------------------------------------------

/// Build the complete system prompt string.
///
/// The returned string contains `SYSTEM_PROMPT_DYNAMIC_BOUNDARY` as an
/// internal marker. It must never be sent to a model: callers split on it
/// with [`split_at_dynamic_boundary`] and send the halves as separate system
/// blocks, the first of which is eligible for Anthropic prompt caching.
pub fn build_system_prompt(opts: &SystemPromptOptions) -> String {
    // Replace mode: skip all default sections.
    if opts.replace_system_prompt {
        if let Some(custom) = &opts.custom_system_prompt {
            return format!("{}\n\n{}", custom, SYSTEM_PROMPT_DYNAMIC_BOUNDARY);
        }
    }

    let prefix = opts
        .prefix
        .unwrap_or_else(|| {
            SystemPromptPrefix::detect(
                opts.is_non_interactive,
                opts.has_append_system_prompt,
            )
        });

    // ------------------------------------------------------------------ //
    // CACHEABLE sections (before the dynamic boundary)                   //
    // ------------------------------------------------------------------ //
    let enabled_tools = opts.enabled_tools.as_deref();
    let mut parts: Vec<String> = vec![
        // 1. Attribution header
        prefix.attribution_text().to_string(),
        // 2. Capabilities: one line per declared tool, then how to approach tasks
        build_capabilities_section(enabled_tools),
        TASK_APPROACH.to_string(),
        // 3. Tool use guidelines (per-tool blocks are conditional on the enabled set)
        build_tool_use_guidelines(enabled_tools),
        // 3b. Platform: one shell-syntax rule for the session's platform
        build_platform_section(
            opts.platform.unwrap_or_else(PromptPlatform::current),
            enabled_tools,
        ),
        // 4. Executing actions with care
        ACTIONS_SECTION.to_string(),
        // 5. Permissions and approvals (the mode itself is in the dynamic section)
        PERMISSIONS_SECTION.to_string(),
        // 5b. Working in the repository, validating, and the final message
        WORKING_STYLE_SECTION.to_string(),
        // 6. Cyber-risk instruction (owned by safeguards — do not edit)
        CYBER_RISK_INSTRUCTION.to_string(),
    ];

    // 7. Coordinator mode (cacheable: content is constant)
    if opts.coordinator_mode {
        parts.push(COORDINATOR_SYSTEM_PROMPT.to_string());
    }

    // 8. Custom system prompt addition (appended to cacheable block)
    if let Some(custom) = &opts.custom_system_prompt {
        parts.push(format!(
            "\n<custom_instructions>\n{}\n</custom_instructions>",
            custom
        ));
    }

    // Dynamic boundary marker
    parts.push(SYSTEM_PROMPT_DYNAMIC_BOUNDARY.to_string());

    // ------------------------------------------------------------------ //
    // DYNAMIC / UNCACHEABLE sections (after the boundary)                //
    // Everything that can differ between sessions or turns goes here so  //
    // the block above stays byte-identical for the prompt cache.         //
    // ------------------------------------------------------------------ //

    // 9. <environment_context>: cwd, shell, platform, date, timezone, git,
    //    workspace roots and permission mode. Its bytes only change when one
    //    of those values changes (a mode switch, /add-dir, the date rolling
    //    over), so later turns reuse the cached message prefix.
    if !opts.skip_env_info {
        parts.push(build_environment_context(opts));
    }

    // 10b. Current permission mode (changes with /yolo and Shift+Tab)
    if let Some(mode) = &opts.permission_mode {
        parts.push(format!("\n{}", permission_mode_sentence(mode)));
    }

    // 11. Session context: git status, IDE context
    if let Some(context) = opts.session_context.as_deref().filter(|c| !c.trim().is_empty()) {
        parts.push(format!("\n{}", context));
    }

    // 12. Output style (dynamic: an inline persona keyword overrides it for
    //     a single turn)
    if let Some(style_text) = opts
        .custom_output_style_prompt
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| opts.output_style.prompt_suffix())
    {
        parts.push(format!("\n## Output Style\n{}", style_text));
    }

    // 13. Memory injection (from memdir)
    if !opts.memory_content.is_empty() {
        parts.push(format!(
            "\n<memory>\n{}\n</memory>",
            opts.memory_content
        ));
    }

    // 14. Active goal addendum (dynamic — changes each turn)
    if let Some(goal_text) = &opts.active_goal_addendum {
        parts.push(goal_text.clone());
    }

    // 15. Appended system prompt: --append-system-prompt plus per-turn
    //     addenda (todo nudge, ultracode procedure, managed-agent prompt)
    if let Some(append) = &opts.append_system_prompt {
        parts.push(format!("\n{}", append));
    }

    parts.join("\n")
}

/// Build the `<environment_context>` block injected after the boundary.
fn build_environment_context(opts: &SystemPromptOptions) -> String {
    let working_dir = opts.working_directory.as_deref();
    // Platform string
    let platform = if cfg!(target_os = "windows") {
        "win32"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };

    // OS version string (mirrors getUnameSR())
    let os_version = {
        #[cfg(target_os = "windows")]
        {
            // Read ProductName from the registry via `ver` or env vars.
            // Also include architecture for clarity.
            let ver = std::process::Command::new("cmd")
                .args(["/c", "ver"])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let arch = std::env::var("PROCESSOR_ARCHITECTURE").unwrap_or_default();
            match ver {
                Some(v) => format!("{} ({})", v, arch),
                None => format!("Windows ({})", arch),
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            // Use uname -sr via std::process for POSIX systems.
            std::process::Command::new("uname")
                .args(["-s", "-r"])
                .output()
                .ok()
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| platform.to_string())
        }
    };

    // Shell detection (mirrors getShellInfoLine())
    let shell_env = std::env::var("SHELL").unwrap_or_default();
    let shell_name = if shell_env.contains("zsh") {
        "zsh"
    } else if shell_env.contains("bash") {
        "bash"
    } else if shell_env.contains("fish") {
        "fish"
    } else if cfg!(target_os = "windows") {
        "powershell"
    } else if shell_env.is_empty() {
        "unknown"
    } else {
        &shell_env
    };

    // Is git repo?
    let is_git = working_dir
        .map(|d| std::path::Path::new(d).join(".git").exists())
        .unwrap_or(false);

    let now = chrono::Local::now();
    let timezone = std::env::var("TZ")
        .ok()
        .filter(|tz| !tz.is_empty())
        .unwrap_or_else(|| now.format("UTC%:z").to_string());
    let workspace_roots = working_dir
        .into_iter()
        .chain(opts.additional_dirs.iter().map(String::as_str))
        .collect::<Vec<_>>()
        .join(", ");

    let mut lines = vec!["\n<environment_context>".to_string()];
    if let Some(cwd) = working_dir {
        lines.push(format!("  <cwd>{cwd}</cwd>"));
    }
    lines.push(format!("  <shell>{shell_name}</shell>"));
    lines.push(format!("  <platform>{platform} ({os_version})</platform>"));
    lines.push(format!("  <current_date>{}</current_date>", now.format("%Y-%m-%d")));
    lines.push(format!("  <timezone>{timezone}</timezone>"));
    lines.push(format!("  <git_repo>{is_git}</git_repo>"));
    if !workspace_roots.is_empty() {
        lines.push(format!("  <workspace_roots>{workspace_roots}</workspace_roots>"));
    }
    if let Some(mode) = &opts.permission_mode {
        lines.push(format!("  <permission_mode>{}</permission_mode>", permission_mode_name(mode)));
    }
    lines.push("</environment_context>".to_string());
    lines.join("\n")
}

/// The settings name of a permission mode (`PermissionMode`'s serde name).
fn permission_mode_name(mode: &crate::config::PermissionMode) -> &'static str {
    use crate::config::PermissionMode;
    match mode {
        PermissionMode::Default => "default",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::Plan => "plan",
        PermissionMode::BypassPermissions => "bypassPermissions",
    }
}

/// Injected as a user message after the user interrupts a turn (Ctrl+C), so
/// the next turn knows the previous one stopped on purpose and may have left
/// work half done.
pub const TURN_ABORTED_MESSAGE: &str = "<turn_aborted>\n\
The user interrupted the previous turn on purpose. Any background Bash commands may still be running. \
If any tools or commands were aborted, they may have partially executed.\n\
</turn_aborted>";

/// Build the platform block for the stable section. Exactly one shell-syntax
/// rule is emitted. On Windows the Bash tool runs commands through `cmd.exe`
/// (see `run_windows_fallback` in the tools crate), so Windows syntax applies.
fn build_platform_section(platform: PromptPlatform, enabled: Option<&[String]>) -> String {
    let rule = match platform {
        PromptPlatform::Linux => {
            "The user is on Linux. Shell commands run in a Unix shell: use POSIX shell \
             syntax and the GNU variants of tools."
        }
        PromptPlatform::MacOs => {
            "The user is on macOS. Shell commands run in a Unix shell: use POSIX shell \
             syntax; the BSD variants of tools apply (e.g., `sed -i ''` not `sed -i`)."
        }
        PromptPlatform::Windows => {
            "The user is on Windows. The Bash tool runs commands through cmd.exe, not a \
             Unix shell: use Windows syntax (`dir` not `ls`, `type` not `cat`, `NUL` not \
             `/dev/null`, backslashes in native paths)."
        }
    };
    let powershell = platform == PromptPlatform::Windows
        && enabled.is_none_or(|names| names.iter().any(|n| n == "PowerShell"));
    let powershell_line = if powershell {
        "\nUse the PowerShell tool for PowerShell syntax and cmdlets."
    } else {
        ""
    };
    format!("\n## Platform\n\n{rule}{powershell_line}\n")
}

// ---------------------------------------------------------------------------
// Static system prompt sections
// ---------------------------------------------------------------------------

/// Tools that get a one-line entry in the capabilities list, in display order,
/// with the summary shown for each. A declared tool not in this table is
/// still usable; its own definition describes it.
const CAPABILITY_SUMMARIES: &[(&str, &str)] = &[
    ("Read", "Read file contents"),
    ("Write", "Create or overwrite files"),
    ("Edit", "Make precise edits with exact text replacement"),
    ("BatchEdit", "Apply several exact text replacements in one atomic call"),
    ("ApplyPatch", "Apply a unified diff to one or more files"),
    ("Glob", "Find files by name pattern"),
    ("Grep", "Search file contents with regular expressions"),
    ("Bash", "Run shell commands"),
    ("PowerShell", "Run PowerShell commands"),
    ("LSP", "Query a configured language server (definitions, references, diagnostics)"),
    ("WebSearch", "Search the web"),
    ("WebFetch", "Fetch and read a URL"),
    ("Agent", "Delegate a bounded task to a sub-agent"),
    ("TodoWrite", "Track a multi-step plan"),
    ("NotebookEdit", "Edit Jupyter notebook cells"),
    ("Skill", "Run a skill"),
    ("AskUserQuestion", "Ask the user to choose between options"),
    ("ToolSearch", "Find a tool by describing the capability you need"),
];

/// Build the "Capabilities" section: one line per declared tool that has a
/// summary in [`CAPABILITY_SUMMARIES`]. With an unknown tool set (`None`) only
/// the general sentence is emitted, so the prompt never claims a tool that is
/// not loaded.
fn build_capabilities_section(enabled: Option<&[String]>) -> String {
    let mut lines = vec![
        String::new(),
        "## Capabilities".to_string(),
        String::new(),
        "Only the tools declared in this request are available; do not claim or call any other."
            .to_string(),
    ];
    if let Some(names) = enabled {
        for (tool, summary) in CAPABILITY_SUMMARIES {
            if names.iter().any(|n| n == tool) {
                lines.push(format!("- {tool}: {summary}"));
            }
        }
        if names
            .iter()
            .any(|n| !CAPABILITY_SUMMARIES.iter().any(|(tool, _)| tool == n))
        {
            lines.push(
                "Other declared tools, such as those from MCP servers, are described by their own definitions."
                    .to_string(),
            );
        }
    }
    lines.push(String::new());
    lines.push(
        "`<system-reminder>` blocks are harness instructions injected by Claurst, not user-authored \
         content and not part of the tool result they appear in. Follow them."
            .to_string(),
    );
    lines.join("\n")
}

const TASK_APPROACH: &str = r#"
## How to approach tasks

1. **Understand before acting**: Read relevant files before making changes
2. **Minimal changes**: Only modify what's needed. Don't refactor unrequested code.
3. **Verify**: Check your work with tests or by running it
4. **Communicate blockers**: If stuck, ask the user rather than guessing
"#;

// ---------------------------------------------------------------------------
// Tool use guidelines (issue #233: progressive tool disclosure)
//
// The section is split into two parts:
//   * general, tool-agnostic guidance that is always emitted, and
//   * per-tool guidance blocks that are emitted only when the corresponding
//     tool is actually enabled for the session.
//
// Emitting only the relevant per-tool blocks removes the fixed prompt tax we
// used to pay for describing tools that aren't even loaded.
// ---------------------------------------------------------------------------

/// Tools we ship a dedicated guideline block for, in the order they should
/// appear.  A tool not in this list simply contributes no per-tool guidance.
const GUIDELINE_TOOLS: &[&str] = &[
    "Bash",
    "Read",
    "Edit",
    "Write",
    "Glob",
    "Grep",
    "WebSearch",
    "WebFetch",
    "Agent",
    "TodoWrite",
    "NotebookEdit",
    "Skill",
    "AskUserQuestion",
];

/// The per-tool guidance line for `tool`, or `None` if we ship no block for it.
fn tool_specific_guideline(tool: &str) -> Option<&'static str> {
    Some(match tool {
        "Bash" => "- Bash commands time out after 2 minutes; use background mode for long-running operations.",
        "Read" => "- Read a file with the Read tool before editing it; Read returns text only, a capped page at a time (continue with offset/limit), and cannot show you image or PDF content.",
        "Edit" => "- For edits, read the file first, then make targeted string replacements with Edit.",
        "Write" => "- Use Write to create new files or fully overwrite; prefer Edit for partial changes.",
        "Glob" => "- To find files by name or pattern, prefer the Glob tool over `find`/`ls`.",
        "Grep" => "- To search file contents, prefer the Grep tool over shelling out to `grep`/`rg`.",
        "WebSearch" => "- Use WebSearch to find current information on the internet.",
        "WebFetch" => "- Use WebFetch to retrieve and read the contents of a specific URL.",
        "Agent" => "- Delegate complex, multi-step, or parallelizable subtasks to sub-agents via the Agent tool.",
        "TodoWrite" => "- Use TodoWrite to plan and track multi-step work; keep exactly one item in_progress.",
        "NotebookEdit" => "- Use NotebookEdit to modify Jupyter (.ipynb) cells instead of editing raw JSON.",
        "Skill" => "- Invoke Skill to run a matching skill/slash-command instead of reimplementing it.",
        "AskUserQuestion" => "- Use AskUserQuestion when the user must choose between options or clarify intent.",
        _ => return None,
    })
}

/// Build the "Tool use guidelines" section.
///
/// `enabled` is the set of tool names active for the session:
/// * `Some(names)` → emit per-tool blocks only for tools in `names`.
/// * `None` → enabled set unknown; emit every per-tool block (backwards-compatible behaviour).
///
/// The general, tool-agnostic guidance is always emitted unchanged.
fn build_tool_use_guidelines(enabled: Option<&[String]>) -> String {
    let mut lines: Vec<String> = vec![
        String::new(),
        "## Tool use guidelines".to_string(),
        String::new(),
        "- Prefer purpose-built tools over raw shell commands when a dedicated tool exists."
            .to_string(),
        "- Parallelize independent tool calls by issuing them together in a single response."
            .to_string(),
    ];
    if enabled.is_none_or(|names| names.iter().any(|n| n == "ToolSearch")) {
        lines.push(
            "- When you're unsure which tool fits a task, use ToolSearch to discover an appropriate one."
                .to_string(),
        );
    }

    for &tool in GUIDELINE_TOOLS {
        let include = match enabled {
            None => true,
            Some(names) => names.iter().any(|n| n == tool),
        };
        if include {
            if let Some(line) = tool_specific_guideline(tool) {
                lines.push(line.to_string());
            }
        }
    }

    lines.push(String::new());
    lines.join("\n")
}

const ACTIONS_SECTION: &str = r#"
## Executing actions with care

Carefully consider the reversibility and blast radius of actions. For actions
that are hard to reverse, affect shared systems, or could be risky or
destructive, check with the user before proceeding. Authorization stands for
the scope specified, not beyond. Match the scope of your actions to what was
actually requested.
"#;

const PERMISSIONS_SECTION: &str = r#"
## Permissions and approvals

Tool calls that change files, run commands or reach the network may need the user's approval, depending on the current permission mode (stated after these instructions). Reading files inside the workspace never needs approval.
- Every Bash call's `description` is shown to the user as the approval question. Write it as a short question the user can answer, e.g. "Install the project's npm dependencies?", not a restatement of the command.
- If a tool call is denied, do not retry it with a different wrapper, shell, or path. Either continue with a safer alternative that does not need the denied capability, or explain what was blocked and ask the user.
- Saved approvals match a tool name, optionally with a glob over the whole Bash command string; a command is not split into segments for matching.
- Never request or rely on broad standing approvals (e.g. "always allow Bash"). Prefer narrow, categorical ones ("cargo test", "npm run dev"). Never ask for a standing approval for destructive commands.
- Potentially destructive actions the user did not explicitly ask for (`rm`, `git reset`, `git checkout --`, dropping data) need a fresh, explicit confirmation from the user, even in a mode that does not prompt.
- Bash commands rated critical risk (e.g. `rm -rf /`, `mkfs`, a download piped into a shell) are refused in every mode.
- Commands run with the user's privileges; there is no OS-level sandbox.
- Do not modify the user's shell or tool configuration (.gitconfig, .bashrc, .zshrc, .mcp.json, .claude.json) unless asked, and never commit secrets, credentials, or API keys.
"#;

const WORKING_STYLE_SECTION: &str = r#"
## Working in the repository

- You may be in a dirty git worktree. NEVER revert changes you did not make unless explicitly asked; if unrelated changes exist in files you are editing, work with them rather than undoing them. If you notice unexpected changes you did not make, stop and ask the user how to proceed.
- Never use destructive git commands (`git reset --hard`, `git checkout --`, force pushes) unless specifically requested or approved. Prefer non-interactive git commands.
- Do not commit or create branches unless asked. Do not amend commits unless asked.
- Do not re-read a file after Edit, Write or another editing tool to check it; the tool fails loudly if it did not apply.
- Fix problems at the root cause; keep changes minimal and in the codebase's style; do not fix unrelated bugs or add tests to codebases that have none (mention them instead).
- Treat unfamiliar files or changes as potential user work and investigate before deleting or overwriting them.
- Keep changes consistent with the structure, naming, style, and patterns of the surrounding code.
- By default, match the surrounding comment density: where the code has none, add none. Use comments only where behaviour is not obvious from the code. User or project instructions take precedence.

## Validating your work

- Start with the most specific test for the code you changed, then broaden. Iterate on formatting at most 3 times; if it still fails, say so.
- In an interactive session that asks before running commands, hold off on long test or lint runs until the user is ready to finalize; suggest them instead. When the task is itself about tests (adding, fixing, reproducing), run them. In non-interactive runs or `bypassPermissions` mode, run them proactively.

## Final message

- Write plain text for a terminal: short paragraphs by default; bullets only when they aid scanning; flat lists; headers only when they genuinely help (short, Title Case, bold).
- Lead with what changed and why; reference files as clickable paths (`src/app.ts:42`), one path per reference, no line ranges.
- Do not paste large files you wrote; the user is on the same machine. Never tell the user to "save" or "copy" a file.
- Brevity is the default: small change, 2-5 sentences; medium, at most 6 bullets; large, 1-2 bullets per file. Offer natural next steps (tests, commit) in one line; do not end with "If you want...".
- If you could not do something (e.g. run tests), say so.
"#;

/// The dynamic-section sentence describing what the current permission mode
/// means. Mirrors `PermissionManager::evaluate`.
fn permission_mode_sentence(mode: &crate::config::PermissionMode) -> &'static str {
    use crate::config::PermissionMode;
    match mode {
        PermissionMode::Default => {
            "Current permission mode: `default`. Reading files inside the workspace is allowed; \
             edits, writes, shell commands and network access ask the user first unless a \
             saved approval covers them (in non-interactive runs, where no one can answer, \
             they are denied)."
        }
        PermissionMode::AcceptEdits => {
            "Current permission mode: `acceptEdits`. Edit calls are approved automatically; \
             writes, shell commands and network access still ask the user first unless a \
             saved approval covers them."
        }
        PermissionMode::Plan => {
            "Current permission mode: `plan`. Read-only: investigate and propose a plan; \
             tools that write, execute commands or reach the network are denied."
        }
        PermissionMode::BypassPermissions => {
            "Current permission mode: `bypassPermissions`. Tool calls run without approval \
             prompts, so exercise the caution the user has delegated to you."
        }
    }
}

/// Security / cyber-risk instruction.
/// No security guardrails injected.
const CYBER_RISK_INSTRUCTION: &str = "";

const COORDINATOR_SYSTEM_PROMPT: &str = r#"
## Coordinator Mode

You are operating as an orchestrator. Spawn parallel worker agents using the Agent tool.
Each worker prompt must be fully self-contained. Synthesize findings before delegating
follow-up work. Use TaskCreate/TaskUpdate to track parallel work.
"#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn default_opts() -> SystemPromptOptions {
        SystemPromptOptions::default()
    }

    #[test]
    fn test_default_prompt_contains_boundary() {
        let prompt = build_system_prompt(&default_opts());
        assert!(
            prompt.contains(SYSTEM_PROMPT_DYNAMIC_BOUNDARY),
            "System prompt must contain the dynamic boundary marker"
        );
    }

    #[test]
    fn split_at_dynamic_boundary_drops_the_marker() {
        let opts = SystemPromptOptions {
            working_directory: Some("/home/user/project".to_string()),
            ..Default::default()
        };
        let prompt = build_system_prompt(&opts);
        let (stable, dynamic) = split_at_dynamic_boundary(&prompt);
        assert!(!stable.contains(SYSTEM_PROMPT_DYNAMIC_BOUNDARY));
        assert!(!dynamic.contains(SYSTEM_PROMPT_DYNAMIC_BOUNDARY));
        assert!(stable.contains("Claurst"));
        assert!(dynamic.contains("/home/user/project"));
        assert!(!stable.contains("/home/user/project"));
        assert_eq!(split_at_dynamic_boundary("no marker"), ("no marker", ""));
    }

    #[test]
    fn test_default_prompt_contains_attribution() {
        let prompt = build_system_prompt(&default_opts());
        assert!(prompt.contains("Claurst"), "Default prompt must contain attribution");
    }

    #[test]
    fn cli_identity_is_provider_neutral() {
        let identity = SystemPromptPrefix::Cli.attribution_text();
        assert!(identity.starts_with("You are Claurst, a coding agent"));
        assert!(!identity.contains("Anthropic"));
        assert!(!identity.contains("Claude"));
    }

    #[test]
    fn test_replace_system_prompt() {
        let opts = SystemPromptOptions {
            custom_system_prompt: Some("Custom only.".to_string()),
            replace_system_prompt: true,
            ..Default::default()
        };
        let prompt = build_system_prompt(&opts);
        assert!(prompt.starts_with("Custom only."));
        assert!(!prompt.contains("Capabilities"));
        assert!(prompt.contains(SYSTEM_PROMPT_DYNAMIC_BOUNDARY));
    }

    #[test]
    fn test_working_directory_in_dynamic_section() {
        let opts = SystemPromptOptions {
            working_directory: Some("/home/user/project".to_string()),
            ..Default::default()
        };
        let prompt = build_system_prompt(&opts);
        let boundary_pos = prompt.find(SYSTEM_PROMPT_DYNAMIC_BOUNDARY).unwrap();
        let cwd_pos = prompt.find("/home/user/project").unwrap();
        assert!(
            cwd_pos > boundary_pos,
            "Working directory must appear after the dynamic boundary"
        );
    }

    #[test]
    fn test_memory_content_in_dynamic_section() {
        let opts = SystemPromptOptions {
            memory_content: "- [test.md](test.md) — a test memory".to_string(),
            ..Default::default()
        };
        let prompt = build_system_prompt(&opts);
        let boundary_pos = prompt.find(SYSTEM_PROMPT_DYNAMIC_BOUNDARY).unwrap();
        let mem_pos = prompt.find("test.md").unwrap();
        assert!(
            mem_pos > boundary_pos,
            "Memory content must appear after the dynamic boundary"
        );
    }

    #[test]
    fn test_output_style_concise() {
        let opts = SystemPromptOptions {
            output_style: OutputStyle::Concise,
            ..Default::default()
        };
        let prompt = build_system_prompt(&opts);
        assert!(prompt.contains("maximally concise"));
    }

    #[test]
    fn output_style_and_session_context_are_dynamic() {
        let opts = SystemPromptOptions {
            output_style: OutputStyle::Concise,
            session_context: Some("# Git Status\n## main".to_string()),
            ..Default::default()
        };
        let prompt = build_system_prompt(&opts);
        let (stable, dynamic) = split_at_dynamic_boundary(&prompt);
        assert!(!stable.contains("maximally concise"));
        assert!(!stable.contains("# Git Status"));
        assert!(dynamic.contains("maximally concise"));
        assert!(dynamic.contains("# Git Status"));
    }

    #[test]
    fn test_output_style_default_has_no_suffix() {
        let opts = SystemPromptOptions {
            output_style: OutputStyle::Default,
            ..Default::default()
        };
        let prompt = build_system_prompt(&opts);
        // None of the style suffixes should appear
        assert!(!prompt.contains("maximally concise"));
        assert!(!prompt.contains("This user is learning"));
    }

    #[test]
    fn test_coordinator_mode_section() {
        let opts = SystemPromptOptions {
            coordinator_mode: true,
            ..Default::default()
        };
        let prompt = build_system_prompt(&opts);
        assert!(prompt.contains("Coordinator Mode"));
        assert!(prompt.contains("orchestrator"));
    }

    #[test]
    fn test_output_style_from_str() {
        assert_eq!(OutputStyle::from_str("concise"), OutputStyle::Concise);
        assert_eq!(OutputStyle::from_str("FORMAL"), OutputStyle::Formal);
        assert_eq!(OutputStyle::from_str("unknown"), OutputStyle::Default);
    }

    #[test]
    fn test_sdk_prefix_non_interactive_no_append() {
        let prefix = SystemPromptPrefix::detect(true, false);
        assert_eq!(prefix, SystemPromptPrefix::Sdk);
        assert!(prefix.attribution_text().contains("Claude agent"));
    }

    #[test]
    fn test_sdk_preset_prefix_non_interactive_with_append() {
        let prefix = SystemPromptPrefix::detect(true, true);
        assert_eq!(prefix, SystemPromptPrefix::SdkPreset);
        assert!(prefix.attribution_text().contains("Claude Agent SDK"));
    }

    #[test]
    fn test_no_enabled_set_emits_all_tool_guidelines() {
        // enabled_tools = None (default) → every per-tool block is emitted.
        let prompt = build_system_prompt(&default_opts());
        assert!(prompt.contains("Bash commands time out"));
        assert!(prompt.contains("prefer the Grep tool"));
        assert!(prompt.contains("prefer the Glob tool"));
        assert!(prompt.contains("Read a file with the Read tool"));
        assert!(prompt.contains("Use WebSearch"));
        // General guidance is always present.
        assert!(prompt.contains("Parallelize independent tool calls"));
    }

    #[test]
    fn test_conditional_tool_guidelines_only_enabled() {
        let opts = SystemPromptOptions {
            enabled_tools: Some(vec!["Read".to_string(), "Edit".to_string()]),
            ..Default::default()
        };
        let prompt = build_system_prompt(&opts);

        // Guidance for enabled tools is present.
        assert!(
            prompt.contains("Read a file with the Read tool"),
            "Read guideline should be emitted"
        );
        assert!(
            prompt.contains("targeted string replacements with Edit"),
            "Edit guideline should be emitted"
        );

        // Guidance for tools NOT in the enabled set is omitted.
        assert!(
            !prompt.contains("Bash commands time out"),
            "Bash guideline must be omitted when Bash is not enabled"
        );
        assert!(
            !prompt.contains("prefer the Grep tool"),
            "Grep guideline must be omitted when Grep is not enabled"
        );
        assert!(
            !prompt.contains("prefer the Glob tool"),
            "Glob guideline must be omitted when Glob is not enabled"
        );
        assert!(
            !prompt.contains("Use WebSearch"),
            "WebSearch guideline must be omitted when WebSearch is not enabled"
        );

        // General, tool-agnostic guidance stays regardless of the enabled set.
        assert!(prompt.contains("## Tool use guidelines"));
        assert!(prompt.contains("Parallelize independent tool calls"));
    }

    #[test]
    fn test_empty_enabled_set_omits_all_tool_specific_guidelines() {
        // Some(empty) is an *explicit* empty set → no per-tool blocks at all,
        // but the general guidance still renders.
        let opts = SystemPromptOptions {
            enabled_tools: Some(vec![]),
            ..Default::default()
        };
        let prompt = build_system_prompt(&opts);
        assert!(prompt.contains("## Tool use guidelines"));
        assert!(prompt.contains("Parallelize independent tool calls"));
        assert!(!prompt.contains("Bash commands time out"));
        assert!(!prompt.contains("Read a file with the Read tool"));
    }

    fn stable_block(opts: &SystemPromptOptions) -> String {
        let prompt = build_system_prompt(opts);
        split_at_dynamic_boundary(&prompt).0.to_string()
    }

    #[test]
    fn capabilities_list_only_declared_tools() {
        let opts = SystemPromptOptions {
            enabled_tools: Some(vec!["Read".to_string(), "Bash".to_string()]),
            ..Default::default()
        };
        let stable = stable_block(&opts);
        assert!(stable.contains("- Read: Read file contents"));
        assert!(stable.contains("- Bash: Run shell commands"));
        for absent in ["- WebSearch:", "- Agent:", "- NotebookEdit:", "Jupyter", "MCP servers", "Memory"] {
            assert!(!stable.contains(absent), "{absent} must not be claimed");
        }
        assert!(!stable.contains("Other declared tools"));
        assert!(!stable.contains("ToolSearch"));
    }

    #[test]
    fn capabilities_mention_undescribed_tools_and_tool_search_when_declared() {
        let opts = SystemPromptOptions {
            enabled_tools: Some(vec!["ToolSearch".to_string(), "github_list_issues".to_string()]),
            ..Default::default()
        };
        let stable = stable_block(&opts);
        assert!(stable.contains("- ToolSearch: Find a tool"));
        assert!(stable.contains("use ToolSearch to discover"));
        assert!(stable.contains("Other declared tools, such as those from MCP servers"));
    }

    #[test]
    fn platform_block_has_one_shell_rule() {
        let unix = stable_block(&SystemPromptOptions {
            platform: Some(PromptPlatform::Linux),
            enabled_tools: Some(vec!["Bash".to_string(), "PowerShell".to_string()]),
            ..Default::default()
        });
        assert_eq!(unix.matches("## Platform").count(), 1);
        assert!(unix.contains("use POSIX shell syntax"));
        assert!(!unix.contains("cmd.exe"));
        assert!(!unix.contains("PowerShell tool for"));

        let windows = stable_block(&SystemPromptOptions {
            platform: Some(PromptPlatform::Windows),
            enabled_tools: Some(vec!["Bash".to_string(), "PowerShell".to_string()]),
            ..Default::default()
        });
        assert_eq!(windows.matches("## Platform").count(), 1);
        assert!(windows.contains("through cmd.exe"));
        assert!(windows.contains("PowerShell tool for"));
        assert!(!windows.contains("Unix shell syntax"));
        assert!(!windows.contains("POSIX"));

        let windows_no_ps = stable_block(&SystemPromptOptions {
            platform: Some(PromptPlatform::Windows),
            enabled_tools: Some(vec!["Bash".to_string()]),
            ..Default::default()
        });
        assert!(!windows_no_ps.contains("PowerShell tool for"));
    }

    #[test]
    fn permission_mode_sentence_is_dynamic_and_stable_block_is_unchanged() {
        use crate::config::PermissionMode;
        let build = |mode: PermissionMode| {
            build_system_prompt(&SystemPromptOptions {
                permission_mode: Some(mode),
                enabled_tools: Some(vec!["Bash".to_string(), "Read".to_string()]),
                platform: Some(PromptPlatform::Linux),
                skip_env_info: true,
                ..Default::default()
            })
        };
        let default_prompt = build(PermissionMode::Default);
        let yolo_prompt = build(PermissionMode::BypassPermissions);
        let (default_stable, default_dynamic) = split_at_dynamic_boundary(&default_prompt);
        let (yolo_stable, yolo_dynamic) = split_at_dynamic_boundary(&yolo_prompt);

        assert_eq!(default_stable, yolo_stable, "the stable block must not depend on the mode");
        assert!(default_stable.contains("## Permissions and approvals"));
        assert!(!default_stable.contains("Current permission mode"));
        assert!(default_dynamic.contains("Current permission mode: `default`"));
        assert!(yolo_dynamic.contains("Current permission mode: `bypassPermissions`"));
        assert_ne!(default_dynamic, yolo_dynamic);
    }

    #[test]
    fn working_style_and_final_message_rules_are_stable() {
        let stable = stable_block(&default_opts());
        for heading in ["## Working in the repository", "## Validating your work", "## Final message"] {
            assert_eq!(stable.matches(heading).count(), 1, "{heading}");
        }
        assert!(stable.contains("NEVER revert changes you did not make"));
        assert!(stable.contains("Do not re-read a file after Edit"));
        assert!(stable.contains("do not end with \"If you want...\""));
        assert!(!stable.contains("by reading the result"));
    }

    #[test]
    fn environment_context_is_dynamic_and_complete() {
        use crate::config::PermissionMode;
        let opts = SystemPromptOptions {
            working_directory: Some("/work/repo".to_string()),
            additional_dirs: vec!["/work/shared".to_string()],
            permission_mode: Some(PermissionMode::AcceptEdits),
            ..Default::default()
        };
        let prompt = build_system_prompt(&opts);
        let (stable, dynamic) = split_at_dynamic_boundary(&prompt);
        assert!(!stable.contains("<environment_context>"));
        assert_eq!(dynamic.matches("<environment_context>").count(), 1);
        for tag in ["<cwd>/work/repo</cwd>", "<shell>", "<platform>", "<current_date>", "<timezone>", "<git_repo>"] {
            assert!(dynamic.contains(tag), "{tag}");
        }
        assert!(dynamic.contains("<workspace_roots>/work/repo, /work/shared</workspace_roots>"));
        assert!(dynamic.contains("<permission_mode>acceptEdits</permission_mode>"));
        assert!(!dynamic.contains("<working_directory>"));
        assert!(!dynamic.contains("<env>"));
    }

    #[test]
    fn harness_semantics_are_stable() {
        let stable = stable_block(&default_opts());
        assert!(stable.contains("`<system-reminder>` blocks are harness instructions"));
        assert!(stable.contains("Treat unfamiliar files or changes as potential user work"));
        assert!(stable.contains("match the surrounding comment density"));
        assert!(stable.contains("patterns of the surrounding code"));
    }

    #[test]
    fn test_clear_section_cache() {
        // Populate cache then clear it — should not panic.
        {
            let mut cache = section_cache().lock().unwrap();
            cache.insert("test_section".to_string(), Some("content".to_string()));
        }
        clear_system_prompt_sections();
        let cache = section_cache().lock().unwrap();
        assert!(cache.is_empty());
    }
}
