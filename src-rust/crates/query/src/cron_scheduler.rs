// cron_scheduler: background task that fires cron-scheduled prompts.
//
// Runs as a long-lived tokio task. Every minute it checks the global CRON_STORE
// (in cc-tools) for tasks whose cron expression matches the current wall-clock
// minute. Matching tasks are fired by spawning a sub-query loop, exactly like
// the AgentTool does for sub-agents.
//
// One-shot tasks (recurring=false) are automatically removed from the store
// by `pop_due_tasks` after they are returned.

use crate::{QueryConfig, QueryOutcome, run_query_loop};
use claurst_core::types::Message;
use claurst_tools::Tool;
use claurst_tools::ToolContext;
use chrono::Timelike;
use std::sync::Arc;
use tokio::time::{Duration, sleep};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

/// Resolves the tool list for a fired cron task. Called when the task fires,
/// not once at startup, so tasks pick up MCP tools whose servers connected
/// after the session started (issue #471).
pub type ToolListProvider = Arc<dyn Fn() -> Arc<Vec<Box<dyn Tool>>> + Send + Sync>;

/// Start the background cron scheduler.
///
/// Returns immediately; the scheduler runs as a detached tokio task.
/// Call `cancel.cancel()` to stop it gracefully.
pub fn start_cron_scheduler(
    client: Arc<claurst_api::AnthropicClient>,
    tool_list: ToolListProvider,
    tool_ctx: ToolContext,
    query_config: QueryConfig,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        run_scheduler_loop(client, tool_list, tool_ctx, query_config, cancel).await;
    });
}

async fn run_scheduler_loop(
    client: Arc<claurst_api::AnthropicClient>,
    tool_list: ToolListProvider,
    tool_ctx: ToolContext,
    query_config: QueryConfig,
    cancel: CancellationToken,
) {
    info!("Cron scheduler started");

    loop {
        // Sleep until the next whole-minute boundary (±1s tolerance).
        let now = chrono::Local::now();
        let secs_into_minute = now.second() as u64;
        let nanos_ms = now.nanosecond() as u64 / 1_000_000;
        // How many ms until the next minute starts? Use saturating sub to avoid underflow.
        let ms_to_next_minute = (60u64.saturating_sub(secs_into_minute))
            .saturating_mul(1_000)
            .saturating_sub(nanos_ms)
            .max(1); // always sleep at least 1ms

        tokio::select! {
            _ = sleep(Duration::from_millis(ms_to_next_minute)) => {}
            _ = cancel.cancelled() => {
                info!("Cron scheduler stopped");
                return;
            }
        }

        let tick_time = chrono::Local::now();
        debug!(time = %tick_time.format("%H:%M"), "Cron scheduler tick");

        // Find tasks due at this minute.
        let due = claurst_tools::cron::pop_due_tasks(&tick_time).await;

        fire_due_tasks(
            client.clone(),
            due,
            &tool_list,
            tool_ctx.clone(),
            query_config.clone(),
            cancel.clone(),
        )
        .await;
    }
}

/// Fire each due task by spawning its own query loop. The tool list is
/// resolved from `tool_list` per task, at fire time (issue #471).
pub(crate) async fn fire_due_tasks(
    client: Arc<claurst_api::AnthropicClient>,
    due: Vec<claurst_tools::cron::CronTask>,
    tool_list: &ToolListProvider,
    tool_ctx: ToolContext,
    query_config: QueryConfig,
    cancel: CancellationToken,
) {
    for task in due {
        info!(id = %task.id, cron = %task.cron, "Firing cron task");

        // Resolve the tool list for this fire: behind an indirection so
        // late-connecting MCP servers are included (issue #471).
        let tools = tool_list();
        let client = client.clone();
        let tool_ctx = tool_ctx.clone();
        let query_config = query_config.clone();
        let cost_tracker = tool_ctx.cost_tracker.clone();
        let cancel_child = cancel.clone();
        let task_id = task.id.clone();

        tokio::spawn(async move {
            fire_one_task(
                client, task, &tools, tool_ctx, query_config, cost_tracker, cancel_child, task_id,
            )
            .await;
        });
    }
}

/// Run a single fired task to completion and record the outcome.
async fn fire_one_task(
    client: Arc<claurst_api::AnthropicClient>,
    task: claurst_tools::cron::CronTask,
    tools: &Arc<Vec<Box<dyn Tool>>>,
    tool_ctx: ToolContext,
    query_config: QueryConfig,
    cost_tracker: Arc<claurst_core::cost::CostTracker>,
    cancel_child: CancellationToken,
    task_id: String,
) {
    let mut messages = vec![Message::user(task.prompt.clone())];

    let outcome = run_query_loop(
        client.as_ref(),
        &mut messages,
        tools,
        &tool_ctx,
        &query_config,
        cost_tracker,
        None, // background — no UI event channel
        cancel_child,
        None, // no pending message queue for cron tasks
    )
    .await;

    match outcome {
        QueryOutcome::EndTurn { .. } => {
            info!(id = %task_id, "Cron task completed");
        }
        QueryOutcome::Error(e) => {
            error!(id = %task_id, error = %e, "Cron task failed");
        }
        QueryOutcome::MaxTokens { .. } => {
            info!(id = %task_id, "Cron task hit max tokens");
        }
        QueryOutcome::Cancelled => {
            debug!(id = %task_id, "Cron task cancelled");
        }
        QueryOutcome::BudgetExceeded { cost_usd, limit_usd } => {
            eprintln!(
                "[cron] task {} budget exceeded: spent ${:.4} of ${:.4}",
                task_id, cost_usd, limit_usd
            );
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use claurst_core::config::PermissionMode;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    struct DenyAll;

    impl claurst_core::permissions::PermissionHandler for DenyAll {
        fn check_permission(
            &self,
            _request: &claurst_core::permissions::PermissionRequest,
        ) -> claurst_core::permissions::PermissionDecision {
            claurst_core::permissions::PermissionDecision::Ask {
                reason: "denied by cron test handler".to_string(),
            }
        }
        fn request_permission(
            &self,
            request: &claurst_core::permissions::PermissionRequest,
        ) -> claurst_core::permissions::PermissionDecision {
            self.check_permission(request)
        }
    }

    fn test_ctx() -> ToolContext {
        ToolContext {
            working_dir: std::path::PathBuf::from("."),
            permission_mode: PermissionMode::Default,
            permission_handler: Arc::new(DenyAll),
            cost_tracker: claurst_core::cost::CostTracker::new(),
            session_id: "cron-test".to_string(),
            file_history: Arc::new(parking_lot::Mutex::new(
                claurst_core::file_history::FileHistory::new(),
            )),
            current_turn: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            non_interactive: true,
            mcp_manager: None,
            config: claurst_core::config::Config::default(),
            managed_agent_config: None,
            completion_notifier: None,
            pending_permissions: None,
            permission_manager: None,
            user_question_tx: None,
            cancel_token: tokio_util::sync::CancellationToken::new(),
            batch_rejection: Default::default(),
        }
    }

    fn task(id: &str) -> claurst_tools::cron::CronTask {
        claurst_tools::cron::CronTask {
            id: id.to_string(),
            cron: "*/5 * * * *".to_string(),
            prompt: "noop".to_string(),
            recurring: true,
            durable: false,
            created_at: 0,
        }
    }

    /// The tool list is resolved per fire, not per scheduler start (issue
    /// #471): each fired task must call the provider, so MCP tools whose
    /// servers connected after the session began are picked up.
    #[tokio::test]
    async fn test_fired_tasks_resolve_their_tool_list_at_fire_time() {
        let fires = Arc::new(AtomicUsize::new(0));
        let tool_list: ToolListProvider = {
            let fires = Arc::clone(&fires);
            Arc::new(move || {
                fires.fetch_add(1, AtomicOrdering::SeqCst);
                Arc::new(Vec::new() as Vec<Box<dyn Tool>>)
            })
        };
        let client = claurst_api::AnthropicClient::new(claurst_api::client::ClientConfig {
            api_key: "test-key".to_string(),
            ..Default::default()
        })
        .expect("build test client");
        // Cancel up front so the spawned query loops stop early; the tool
        // list is resolved before they are spawned.
        let cancel = CancellationToken::new();
        cancel.cancel();

        fire_due_tasks(
            Arc::new(client),
            vec![task("a"), task("b")],
            &tool_list,
            test_ctx(),
            QueryConfig::default(),
            cancel,
        )
        .await;

        assert_eq!(
            fires.load(AtomicOrdering::SeqCst),
            2,
            "each fired task must resolve its tool list"
        );
    }
}
