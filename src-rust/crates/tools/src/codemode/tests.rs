// Integration tests for the opt-in codemode tool: the sandbox calls tools
// through `execute_tool_dispatch`, so permissions and tool gating apply to
// script sub-calls the same as to top-level calls.

use crate::{CodemodeTool, Tool, ToolContext, ToolResult};
use async_trait::async_trait;
use claurst_codemode::{Host, Limits, ToolInfo};
use claurst_core::permissions::{PermissionDecision, PermissionHandler, PermissionRequest};
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Serializes tests that mutate the codemode feature-gate env vars, which are
/// process-wide and would otherwise race with each other.
static CODEMODE_ENV_LOCK: once_cell::sync::Lazy<tokio::sync::Mutex<()>> =
    once_cell::sync::Lazy::new(|| tokio::sync::Mutex::new(()));

/// Runs `f` with both codemode env vars removed and the gate mutex held.
async fn with_codemode_gate<R>(f: impl std::future::Future<Output = R>) -> R {
    let _guard = CODEMODE_ENV_LOCK.lock().await;
    std::env::remove_var("CLAURST_FEATURE_CODEMODE");
    std::env::remove_var("CLAURST_CODEMODE");
    f.await
}

struct AllowAllHandler;

impl PermissionHandler for AllowAllHandler {
    fn check_permission(&self, _request: &PermissionRequest) -> PermissionDecision {
        PermissionDecision::Allow
    }
    fn request_permission(&self, request: &PermissionRequest) -> PermissionDecision {
        self.check_permission(request)
    }
}

struct DenyAllHandler;

impl PermissionHandler for DenyAllHandler {
    fn check_permission(&self, _request: &PermissionRequest) -> PermissionDecision {
        PermissionDecision::Deny
    }
    fn request_permission(&self, request: &PermissionRequest) -> PermissionDecision {
        self.check_permission(request)
    }
}

/// Counting read-only probe tool: every dispatch reaches `execute`.
struct ProbeTool {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for ProbeTool {
    fn name(&self) -> &str {
        "Probe"
    }
    fn description(&self) -> &str {
        "Test probe tool"
    }
    fn permission_level(&self) -> crate::PermissionLevel {
        crate::PermissionLevel::ReadOnly
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    async fn execute(&self, input: Value, _ctx: &ToolContext) -> ToolResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        ToolResult::success(input.to_string())
    }
}

/// Write-level tool that never runs when the backstop denies it.
struct WriteProbeTool;

#[async_trait]
impl Tool for WriteProbeTool {
    fn name(&self) -> &str {
        "WriteProbe"
    }
    fn description(&self) -> &str {
        "Test write tool"
    }
    fn permission_level(&self) -> crate::PermissionLevel {
        crate::PermissionLevel::Write
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    async fn execute(&self, _input: Value, _ctx: &ToolContext) -> ToolResult {
        ToolResult::success("wrote")
    }
}

fn test_tool_context(handler: Arc<dyn PermissionHandler>) -> ToolContext {
    use claurst_core::config::Config;

    ToolContext {
        working_dir: PathBuf::from("/workspace"),
        permission_mode: claurst_core::config::PermissionMode::Default,
        permission_handler: handler,
        cost_tracker: claurst_core::cost::CostTracker::new(),
        session_id: "codemode-test".to_string(),
        file_history: Arc::new(parking_lot::Mutex::new(
            claurst_core::file_history::FileHistory::new(),
        )),
        current_turn: Arc::new(AtomicUsize::new(0)),
        non_interactive: true,
        mcp_manager: None,
        config: Config::default(),
        managed_agent_config: None,
        completion_notifier: None,
        pending_permissions: None,
        permission_manager: None,
        user_question_tx: None,
        cancel_token: tokio_util::sync::CancellationToken::new(),
        batch_rejection: Default::default(),
    }
}

fn boxed_tools(tools: Vec<Box<dyn Tool>>) -> Vec<Box<dyn Tool>> {
    tools
}

/// Sandbox host built over the claurst tool dispatch path, as the CodemodeTool
/// does internally. Mirrors `codemode::RegistryHost` for the test.
struct DispatchHost {
    tools: Arc<Vec<Box<dyn Tool>>>,
    ctx: ToolContext,
}

impl Host for DispatchHost {
    fn call_tool(
        &self,
        name: String,
        args: Value,
    ) -> futures::future::BoxFuture<'static, Result<Value, String>> {
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

async fn run_script(
    code: &str,
    ctx: ToolContext,
    tools: Vec<Box<dyn Tool>>,
) -> claurst_codemode::RunOutput {
    let infos: Vec<ToolInfo> = tools
        .iter()
        .map(|t| ToolInfo {
            name: t.name().to_string(),
            description: t.description().to_string(),
            input_schema: t.input_schema(),
        })
        .collect();
    let host = Arc::new(DispatchHost {
        tools: Arc::new(boxed_tools(tools)),
        ctx,
    });
    claurst_codemode::run(code.to_string(), host, infos, Map::new(), Limits::default()).await
}

#[tokio::test(flavor = "multi_thread")]
async fn script_calls_tool_through_dispatch() {
    let calls = Arc::new(AtomicUsize::new(0));
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(ProbeTool {
        calls: calls.clone(),
    })];
    let ctx = test_tool_context(Arc::new(AllowAllHandler));
    let out = run_script(
        "const r = await tools.Probe({ a: 1 }); return r;",
        ctx,
        tools,
    )
    .await;
    assert_eq!(out.error, None, "{:?}", out.error);
    assert_eq!(out.result, Some(json!({ "a": 1 })));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn permission_backstop_blocks_gated_subcall() {
    // Non-interactive context: an Ask decision is rejected, and the Write-level
    // sub-call must never reach execute.
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(WriteProbeTool)];
    let ctx = test_tool_context(Arc::new(DenyAllHandler));
    let out = run_script(
        "try { await tools.WriteProbe({}); return \"ran\"; } catch (e) { return String(e.message); }",
        ctx,
        tools,
    )
    .await;
    assert_eq!(out.error, None, "{:?}", out.error);
    let msg = out.result.unwrap_or_default();
    assert!(
        msg.as_str().unwrap_or("").contains("Permission denied"),
        "{msg}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_tool_is_reported_to_the_script() {
    let tools: Vec<Box<dyn Tool>> = vec![];
    let ctx = test_tool_context(Arc::new(AllowAllHandler));
    let out = run_script(
        "try { await tools.Nope({}); return \"ran\"; } catch (e) { return e.message; }",
        ctx,
        tools,
    )
    .await;
    assert_eq!(out.error, None);
    let msg = out.result.unwrap_or_default();
    assert!(
        msg.as_str().unwrap_or("").contains("unknown tool 'Nope'"),
        "{msg}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn disabled_by_default() {
    // No env var set in the test process: the tool must refuse to run.
    with_codemode_gate(async {
        let ctx = test_tool_context(Arc::new(AllowAllHandler));
        let result = CodemodeTool
            .execute(json!({ "code": "return 1;" }), &ctx)
            .await;
        assert!(result.is_error, "codemode must be opt-in");
        assert!(result.content.contains("disabled"), "{}", result.content);
    })
    .await
}

/// The registry only offers the codemode tool while its feature gate is on;
/// with the gate off the tool stays invisible, so default-off is preserved.
#[tokio::test(flavor = "multi_thread")]
async fn registered_only_when_enabled() {
    let _guard = CODEMODE_ENV_LOCK.lock().await;

    std::env::set_var("CLAURST_CODEMODE", "1");
    let names: Vec<String> = crate::all_tools()
        .into_iter()
        .map(|t| t.name().to_string())
        .collect();
    assert!(names.contains(&"codemode".to_string()), "{names:?}");
    assert!(crate::find_tool("codemode").is_some());

    std::env::remove_var("CLAURST_CODEMODE");
    let names: Vec<String> = crate::all_tools()
        .into_iter()
        .map(|t| t.name().to_string())
        .collect();
    assert!(!names.contains(&"codemode".to_string()), "{names:?}");
    assert!(crate::find_tool("codemode").is_none());
}
