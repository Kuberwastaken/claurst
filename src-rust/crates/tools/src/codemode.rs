// Opt-in `codemode` tool: run model-written JavaScript in an embedded QuickJS
// sandbox that can call any tool available to the session.
//
// Enabled with `CLAURST_FEATURE_CODEMODE=1` or `CLAURST_CODEMODE=1`; default
// off. The sandbox itself lives in the `claurst-codemode` crate; this module
// is the harness side: it lists the session's tools, routes calls through the
// tool dispatch path (so permissions, deny rules, and hooks all apply), and
// keeps the script `store()` per session between runs.

use crate::{PermissionLevel, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use claurst_codemode::{Host, Limits, ToolInfo};
use futures::future::BoxFuture;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Tools never reachable from inside a script: itself (no recursion) and
/// `batch`-style parallel tools are covered by `Promise.all` already.
const EXCLUDED: &[&str] = &["codemode"];

/// Maximum characters of the final report returned to the model.
const MAX_REPORT_CHARS: usize = 60_000;

/// Tool name of the sandboxed batch runner.
pub const TOOL_NAME_CODEMODE: &str = "codemode";

const DESCRIPTION: &str = r#"Run a JavaScript program in a sandbox that can call any of your other tools, including MCP tools. Use it to chain, loop over, filter, or parallelize many tool calls in one step, so intermediate results never enter the conversation. Only what you `return` (plus console output) comes back.

The code is the body of an async function: use `await` and `return`.
- `await tools.<name>(args)` calls a tool with the same arguments it normally takes. JSON output is parsed into objects, other output is a string. Failures throw an Error.
- `await callTool(name, args)` does the same for names that are not valid identifiers.
- `listTools(filter?)` returns `[{name, description}]`. `describeTool(name)` returns the full description and input_schema. Call these first if unsure of a tool's arguments.
- `store(key, value)` / `load(key)` / `storedKeys()` keep JSON values across codemode calls in this session. Use them to avoid refetching.
- `console.log(...)` output is returned along with the result.
- Run independent calls in parallel with `Promise.all`.
No filesystem, network, timers, or modules exist except through tools. Limits: 64 MB heap, 300 s, 2000 tool calls per run.

Example:
const files = (await tools.Bash({ command: "git ls-files '*.rs'" })).trim().split("\n");
const hits = await Promise.all(files.slice(0, 50).map(async (f) => {
  const text = await tools.Read({ file_path: f, limit: 400 });
  return { f, todos: (text.match(/TODO/g) || []).length };
}));
store("todo_counts", hits);
return hits.filter((h) => h.todos > 0).sort((a, b) => b.todos - a.todos).slice(0, 10);"#;

pub struct CodemodeTool;

/// Whether the opt-in codemode tool is enabled. Opt-in via the
/// `CLAURST_FEATURE_CODEMODE=1` feature gate or `CLAURST_CODEMODE=1`,
/// defaulting to off.
pub fn codemode_enabled() -> bool {
    claurst_codemode::AVAILABLE
        && (claurst_core::feature_gates::is_feature_enabled("codemode")
            || claurst_core::feature_gates::is_env_truthy(
                std::env::var("CLAURST_CODEMODE").ok().as_deref(),
            ))
}

#[derive(Deserialize)]
struct CodemodeInput {
    #[serde(alias = "script", alias = "source", alias = "js")]
    code: String,
}

/// Harness side of the sandbox: owns the tool set for one run and dispatches
/// every script tool call through [`crate::execute_tool_dispatch`], which
/// applies the tool-wide deny rules and the central permission backstop the
/// same way a top-level tool call is gated.
struct RegistryHost {
    tools: Arc<Vec<Box<dyn Tool>>>,
    ctx: ToolContext,
}

impl Host for RegistryHost {
    fn call_tool(&self, name: String, args: Value) -> BoxFuture<'static, Result<Value, String>> {
        let tools = self.tools.clone();
        let ctx = self.ctx.clone();
        Box::pin(async move {
            let args = match args {
                Value::Object(map) => Value::Object(map),
                other => return Err(format!("arguments must be an object, got {other}")),
            };
            let result = crate::execute_tool_dispatch(&name, &args, &tools, &ctx).await;
            if result.is_error {
                Err(result.content)
            } else {
                Ok(claurst_codemode::decode_tool_output(&result.content))
            }
        })
    }
}

fn store_path(session_id: &str) -> Option<PathBuf> {
    let safe: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        return None;
    }
    Some(
        claurst_core::config::Settings::config_dir()
            .join("codemode")
            .join(format!("{safe}.json")),
    )
}

fn load_store(session_id: &str) -> Map<String, Value> {
    store_path(session_id)
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<Map<String, Value>>(&text).ok())
        .unwrap_or_default()
}

fn save_store(session_id: &str, store: &Map<String, Value>) {
    let Some(path) = store_path(session_id) else {
        return;
    };
    if store.is_empty() {
        let _ = std::fs::remove_file(&path);
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, Value::Object(store.clone()).to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn truncate(text: String, max: usize) -> String {
    if text.chars().count() <= max {
        return text;
    }
    let cut: String = text.chars().take(max).collect();
    format!("{cut}\n...(truncated; return a smaller value or store() the rest)")
}

/// Render a sandbox run for the model: console output, then the result or
/// error, then a footer with the call count, duration, and stored keys.
pub fn render_report(out: &claurst_codemode::RunOutput) -> String {
    let mut report = String::new();
    if !out.logs.is_empty() {
        report.push_str("console:\n");
        for line in &out.logs {
            report.push_str(line);
            report.push('\n');
        }
        if out.logs_truncated {
            report.push_str("...(console output truncated)\n");
        }
        report.push('\n');
    }
    match (&out.error, &out.result) {
        (Some(err), _) => {
            report.push_str("error: ");
            report.push_str(err);
            report.push('\n');
        }
        (None, Some(value)) => {
            report.push_str("result:\n");
            let rendered = match value {
                Value::String(s) => s.clone(),
                other => serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
            };
            report.push_str(&rendered);
            report.push('\n');
        }
        (None, None) => report.push_str("result: undefined\n"),
    }
    let mut footer = format!(
        "\n[{} tool call{}, {:.1}s",
        out.tool_calls,
        if out.tool_calls == 1 { "" } else { "s" },
        out.elapsed.as_secs_f64()
    );
    if !out.store.is_empty() {
        let keys: Vec<&str> = out.store.keys().map(String::as_str).take(20).collect();
        footer.push_str(&format!(", stored: {}", keys.join(", ")));
    }
    footer.push(']');
    report.push_str(&footer);
    truncate(report, MAX_REPORT_CHARS)
}

/// Build the sandbox host over the session's tools. The sandbox calls every
/// tool through the normal dispatch, so permissions, deny rules, and the
/// central permission backstop apply to each sub-call.
fn codemode_host(tools: Vec<Box<dyn Tool>>, ctx: &ToolContext) -> RegistryHost {
    RegistryHost {
        tools: Arc::new(tools),
        ctx: ctx.clone(),
    }
}

/// Tool metadata (`ToolInfo`) for the tools the script may call.
pub fn codemode_tool_infos(tools: &[Box<dyn Tool>]) -> Vec<ToolInfo> {
    tools
        .iter()
        .filter(|tool| !EXCLUDED.contains(&tool.name()))
        .map(|tool| ToolInfo {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            input_schema: tool.input_schema(),
        })
        .collect()
}

#[async_trait]
impl Tool for CodemodeTool {
    fn name(&self) -> &str {
        TOOL_NAME_CODEMODE
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn permission_level(&self) -> PermissionLevel {
        PermissionLevel::Execute
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["code"],
            "properties": {
                "code": {
                    "type": "string",
                    "description": "Body of an async JavaScript function. Use await and return."
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> ToolResult {
        if !codemode_enabled() {
            return ToolResult::error(
                "codemode is disabled. Enable it with CLAURST_FEATURE_CODEMODE=1.",
            );
        }
        let params: CodemodeInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(format!("Invalid input: {}", e)),
        };
        if params.code.trim().is_empty() {
            return ToolResult::error("code is empty");
        }

        // The session's active tool set, minus the ones a script must not reach.
        let tools: Vec<Box<dyn Tool>> = crate::all_tools()
            .into_iter()
            .filter(|tool| !EXCLUDED.contains(&tool.name()))
            .collect();
        let infos = codemode_tool_infos(&tools);

        let host = Arc::new(codemode_host(tools, ctx));
        let store = load_store(&ctx.session_id);
        let limits = Limits {
            timeout: Duration::from_secs(300),
            ..Limits::default()
        };
        let out = claurst_codemode::run(params.code, host, infos, store, limits).await;
        save_store(&ctx.session_id, &out.store);

        let report = render_report(&out);
        if out.error.is_some() && out.tool_calls == 0 && out.logs.is_empty() {
            // Nothing happened: surface as a tool error so the model retries.
            return ToolResult::error(report);
        }
        ToolResult::success(report)
    }
}

#[cfg(test)]
mod tests;
