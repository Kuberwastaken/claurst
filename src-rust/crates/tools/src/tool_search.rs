// ToolSearchTool: discover tools by name or keyword.
//
// The model uses this to find the right tool for a task, or to look up a
// tool it half-remembers. The catalog is built from the *actually registered*
// tools (`all_tools()`), so it always reflects real tool names and their
// one-line descriptions instead of a hand-maintained list that can drift.
//
// Supports two query modes:
//   - "select:ToolName[,Other]" → direct lookup by exact name(s)
//   - "keyword search"          → ranked name + description + keyword match

use crate::{all_tools, PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

pub struct ToolSearchTool;

#[derive(Debug, Deserialize)]
struct ToolSearchInput {
    query: String,
    #[serde(default = "default_max")]
    max_results: usize,
}

fn default_max() -> usize {
    5
}

/// A catalog entry describing one searchable tool.
struct CatalogEntry {
    name: String,
    description: String,
    keywords: &'static [&'static str],
}

/// Extra search synonyms for high-value tools, keyed by canonical name.
/// These improve recall for natural-language queries (e.g. "search the web").
fn keywords_for(name: &str) -> &'static [&'static str] {
    match name {
        "Bash" => &["shell", "run", "command", "exec", "terminal"],
        "Read" => &["file", "cat", "content", "open"],
        "Write" => &["file", "create", "save", "new"],
        "Edit" => &["file", "modify", "replace", "patch", "change"],
        "Glob" => &["find", "pattern", "files", "filename"],
        "Grep" => &["search", "regex", "content", "ripgrep"],
        "WebFetch" => &["web", "url", "http", "download", "browse", "internet"],
        "WebSearch" => &["web", "internet", "google", "browse", "news"],
        "NotebookEdit" => &["notebook", "jupyter", "ipynb", "cell"],
        "TodoWrite" => &["todo", "task", "plan", "checklist"],
        "AskUserQuestion" => &["ask", "question", "clarify", "choose"],
        "Agent" => &["agent", "subagent", "delegate", "parallel", "spawn"],
        "Skill" => &["skill", "slash", "command", "template", "prompt"],
        "Config" => &["config", "settings", "model", "permission"],
        "SendMessage" => &["message", "broadcast", "inbox", "communicate"],
        _ => &[],
    }
}

/// Tools that are registered outside `all_tools()` (e.g. the Agent tool lives
/// in the query crate) but should still be discoverable here.
static SUPPLEMENTAL_TOOLS: &[(&str, &str)] = &[(
    "Agent",
    "Launch a sub-agent to handle a complex, multi-step task in parallel.",
)];

/// Collapse a possibly multi-line/verbose description into a single tidy line.
fn one_line(desc: &str) -> String {
    let collapsed = desc.split_whitespace().collect::<Vec<_>>().join(" ");
    // Prefer the first sentence; otherwise cap the length so results stay terse.
    let first_sentence = collapsed
        .find(". ")
        .map(|i| &collapsed[..=i]) // include the period
        .unwrap_or(&collapsed);
    let trimmed = first_sentence.trim_end_matches('.').trim();
    if trimmed.chars().count() > 160 {
        let cut: String = trimmed.chars().take(157).collect();
        format!("{}...", cut.trim_end())
    } else {
        trimmed.to_string()
    }
}

/// Build the searchable catalog from the live tool registry plus supplements.
fn build_catalog() -> Vec<CatalogEntry> {
    let mut entries: Vec<CatalogEntry> = all_tools()
        .iter()
        .map(|t| CatalogEntry {
            name: t.name().to_string(),
            description: one_line(t.description()),
            keywords: keywords_for(t.name()),
        })
        .collect();

    for (name, desc) in SUPPLEMENTAL_TOOLS {
        if !entries.iter().any(|e| e.name == *name) {
            entries.push(CatalogEntry {
                name: (*name).to_string(),
                description: one_line(desc),
                keywords: keywords_for(name),
            });
        }
    }

    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// Score a single catalog entry against the lowercase query terms.
/// Name matches dominate, then keywords, then description hits.
fn score_entry(entry: &CatalogEntry, terms: &[&str]) -> usize {
    let name_lower = entry.name.to_lowercase();
    let desc_lower = entry.description.to_lowercase();
    let mut score = 0usize;

    for term in terms {
        if name_lower == *term {
            score += 25; // exact name match ranks highest
        } else if name_lower.contains(term) {
            score += 10;
        }

        for &kw in entry.keywords {
            if kw == *term {
                score += 8;
            } else if kw.contains(term) {
                score += 3;
            }
        }

        if desc_lower.split_whitespace().any(|w| w.trim_matches(|c: char| !c.is_alphanumeric()) == *term) {
            score += 5; // whole-word description hit
        } else if desc_lower.contains(term) {
            score += 2; // substring description hit
        }
    }

    score
}

#[async_trait]
impl Tool for ToolSearchTool {
    fn name(&self) -> &str {
        "ToolSearch"
    }

    fn description(&self) -> &str {
        "Find the right tool for a task. Search all available tools by name or keyword and \
         get back the best-matching tool names with a one-line description each. Use a natural \
         phrase (e.g. 'search the web', 'edit a file') to discover a capability, or \
         'select:ToolName' for a direct lookup. Returns up to 5 results by default."
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::None
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "A task description or keywords to find a tool, or 'select:ToolName' for a direct lookup"
                },
                "max_results": {
                    "type": "number",
                    "description": "Maximum results to return (default: 5, max: 20)"
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        let params: ToolSearchInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };

        let query = params.query.trim();
        let max = params.max_results.clamp(1, 20);
        let catalog = build_catalog();

        // ---- select: prefix — direct lookup by exact name(s) ----------------
        if let Some(names_str) = query.strip_prefix("select:").map(str::trim) {
            let requested: Vec<&str> = names_str
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            let mut found = Vec::new();
            let mut missing = Vec::new();

            for name in requested {
                if let Some(entry) = catalog
                    .iter()
                    .find(|e| e.name.eq_ignore_ascii_case(name))
                {
                    found.push(format!("{}: {}", entry.name, entry.description));
                } else {
                    missing.push(name.to_string());
                }
            }

            if found.is_empty() {
                return ToolResult::success(format!(
                    "No matching tools found for: {}",
                    missing.join(", ")
                ));
            }

            let mut out = found.join("\n");
            if !missing.is_empty() {
                out.push_str(&format!("\n\nNot found: {}", missing.join(", ")));
            }
            return ToolResult::success(out);
        }

        // ---- keyword search with scoring ------------------------------------
        let q_lower = query.to_lowercase();
        let terms: Vec<&str> = q_lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.len() > 1) // drop empties and single-char noise
            .collect();

        if terms.is_empty() {
            return ToolResult::success(format!(
                "Empty query. Provide keywords or a task description, or use 'select:ToolName'. \
                 {} tools available.",
                catalog.len()
            ));
        }

        let mut scored: Vec<(usize, &CatalogEntry)> = catalog
            .iter()
            .filter_map(|entry| {
                let score = score_entry(entry, &terms);
                if score > 0 {
                    Some((score, entry))
                } else {
                    None
                }
            })
            .collect();

        // Highest score first; break ties by name for deterministic output.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        scored.truncate(max);

        if scored.is_empty() {
            return ToolResult::success(format!(
                "No tools matched '{}'. Try broader keywords or use 'select:ToolName'. \
                 {} tools are available.",
                query,
                catalog.len()
            ));
        }

        let lines: Vec<String> = scored
            .iter()
            .map(|(_, e)| format!("{}: {}", e.name, e.description))
            .collect();

        ToolResult::success(format!(
            "Tools matching '{}' (use one of these for the task):\n\n{}\n\n{} of {} tools shown.",
            query,
            lines.join("\n"),
            scored.len(),
            catalog.len()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ToolContext {
        crate::test_support::allow_all_context(std::env::temp_dir())
    }

    async fn run(query: &str) -> String {
        let tool = ToolSearchTool;
        let out = tool
            .execute(json!({ "query": query }), &ctx())
            .await;
        out.content
    }

    #[tokio::test]
    async fn web_query_surfaces_web_tools() {
        let out = run("search the web").await;
        assert!(
            out.contains("WebSearch"),
            "expected WebSearch in results, got:\n{out}"
        );
        // WebSearch should rank ahead of WebFetch for this query.
        let ws = out.find("WebSearch");
        let wf = out.find("WebFetch");
        if let (Some(ws), Some(wf)) = (ws, wf) {
            assert!(ws < wf, "WebSearch should rank above WebFetch:\n{out}");
        }
    }

    #[tokio::test]
    async fn exact_name_ranks_first() {
        let out = run("grep").await;
        let first_line = out
            .lines()
            .find(|l| l.contains(": "))
            .unwrap_or_default();
        assert!(
            first_line.starts_with("Grep:"),
            "exact name match should rank first, got first result line: {first_line:?}\n{out}"
        );
    }

    #[tokio::test]
    async fn select_prefix_direct_lookup() {
        let out = run("select:WebFetch,DoesNotExist").await;
        assert!(out.contains("WebFetch:"), "should find WebFetch:\n{out}");
        assert!(
            out.contains("Not found: DoesNotExist"),
            "should report the missing tool:\n{out}"
        );
    }

    #[tokio::test]
    async fn agent_is_discoverable_via_supplement() {
        let out = run("delegate a subagent task").await;
        assert!(
            out.contains("Agent"),
            "Agent tool should be discoverable even though it lives outside all_tools():\n{out}"
        );
    }
}
