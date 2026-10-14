// Headless (`-p` / `--print`) run contract (#473).
//
// `--output-format stream-json` writes one JSON object per line, shaped like
// Claude Code's stream-json:
//
//   {"type":"system","subtype":"init","session_id","model","cwd","tools","permissionMode"}
//   {"type":"assistant","message":{"id","role","content":[...]},"parent_tool_use_id":null,"session_id"}
//   {"type":"user","message":{"role","content":[tool_result...]},"parent_tool_use_id":null,"session_id"}
//   {"type":"retry","attempt","max_attempts","delay_ms","reason","session_id"}
//   {"type":"compacted","trigger","messages_before",...,"session_id"}
//   {"type":"result","subtype",...}
//
// Assistant and user events carry complete content blocks (text, thinking,
// tool_use with its id, tool_result with its tool_use_id) and are emitted once
// per message after any retry has settled. `--output-format json` writes only
// the final `result` object. Text mode prints each assistant message's text
// once it is complete, so output streamed by a failed attempt never reaches
// stdout.

use claurst_core::history::CompactionRecord;
use claurst_core::types::{ContentBlock, Message, MessageContent, Role};
use claurst_query::{QueryEvent, QueryOutcome};
use serde::Serialize;
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use crate::CliOutputFormat;

/// Why the run ended, as reported in the `result` event's `subtype`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResultSubtype {
    /// The model ended its turn.
    Success,
    /// The turn cap (`--max-turns` / agent `max_turns`) was reached.
    ErrorMaxTurns,
    /// `--max-budget-usd` was reached.
    ErrorMaxBudgetUsd,
    /// The response hit the output-token limit after recovery attempts.
    ErrorMaxTokens,
    /// The run was interrupted (Ctrl-C).
    Cancelled,
    /// An unrecoverable error ended the run.
    ErrorDuringExecution,
}

impl ResultSubtype {
    /// Process exit code: 0 on success, 1 on error, 2 when a limit (turns,
    /// budget, output tokens) stopped the run, 130 on Ctrl-C.
    pub(crate) fn exit_code(self) -> i32 {
        match self {
            Self::Success => 0,
            Self::ErrorDuringExecution => 1,
            Self::ErrorMaxTurns | Self::ErrorMaxBudgetUsd | Self::ErrorMaxTokens => 2,
            Self::Cancelled => 130,
        }
    }
}

/// Cumulative token usage for the run.
#[derive(Debug, Serialize)]
pub(crate) struct ResultUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

/// The final `result` event (and the whole output in `json` mode).
#[derive(Debug, Serialize)]
pub(crate) struct RunResult {
    pub subtype: ResultSubtype,
    pub is_error: bool,
    /// Text of the last assistant message.
    pub result: String,
    /// `end_turn`, `max_turns`, `budget`, `max_tokens`, `cancelled` or `error`.
    pub stop_reason: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub session_id: String,
    /// Assistant messages produced by this run.
    pub num_turns: u32,
    pub duration_ms: u64,
    pub total_cost_usd: f64,
    pub usage: ResultUsage,
    /// Input of the accepted StructuredOutput call (`--json-schema` runs).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<serde_json::Value>,
}

/// A message as carried by `assistant` / `user` events.
#[derive(Debug, Serialize)]
pub(crate) struct EventMessage<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<&'a str>,
    role: &'a Role,
    content: Cow<'a, [ContentBlock]>,
}

impl<'a> EventMessage<'a> {
    fn new(message: &'a Message) -> Self {
        let content = match &message.content {
            MessageContent::Blocks(blocks) => Cow::Borrowed(blocks.as_slice()),
            MessageContent::Text(text) => Cow::Owned(vec![ContentBlock::Text { text: text.clone() }]),
        };
        Self { id: message.uuid.as_deref(), role: &message.role, content }
    }
}

/// One line of `stream-json` output.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum HeadlessEvent<'a> {
    System {
        subtype: &'static str,
        session_id: &'a str,
        model: &'a str,
        cwd: &'a str,
        tools: Vec<&'a str>,
        /// Claude Code's SDKSystemMessage spells this field in camelCase.
        #[serde(rename = "permissionMode")]
        permission_mode: &'a claurst_core::config::PermissionMode,
    },
    /// `parent_tool_use_id` is always null: sub-agent messages are not
    /// streamed. Kept (serialized as null) because Claude Code consumers
    /// filter top-level messages on `parent_tool_use_id === null`.
    Assistant {
        message: EventMessage<'a>,
        parent_tool_use_id: Option<&'a str>,
        session_id: &'a str,
    },
    User {
        message: EventMessage<'a>,
        parent_tool_use_id: Option<&'a str>,
        session_id: &'a str,
    },
    Retry {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        reason: &'a str,
        session_id: &'a str,
    },
    Compacted {
        #[serde(flatten)]
        record: &'a CompactionRecord,
        session_id: &'a str,
    },
    Result(&'a RunResult),
}

/// Session facts reported by the `system` init event.
pub(crate) struct InitInfo<'a> {
    pub model: &'a str,
    pub cwd: &'a str,
    pub tools: Vec<&'a str>,
    pub permission_mode: &'a claurst_core::config::PermissionMode,
}

/// Turns `QueryEvent`s into the output of the selected format and collects
/// what the result event and the saved session need.
pub(crate) struct HeadlessSink<W: Write, E: Write> {
    format: CliOutputFormat,
    session_id: String,
    out: W,
    err: E,
    num_turns: u32,
    last_assistant_text: String,
    printed_text: bool,
    max_turns: Option<u32>,
    compactions: Vec<CompactionRecord>,
    /// Inputs of StructuredOutput calls awaiting their tool result, by id.
    pending_structured: HashMap<String, serde_json::Value>,
    /// Input of the last StructuredOutput call the tool accepted.
    structured_output: Option<serde_json::Value>,
}

impl<W: Write, E: Write> HeadlessSink<W, E> {
    pub(crate) fn new(format: CliOutputFormat, session_id: String, out: W, err: E) -> Self {
        Self {
            format,
            session_id,
            out,
            err,
            num_turns: 0,
            last_assistant_text: String::new(),
            printed_text: false,
            max_turns: None,
            compactions: Vec::new(),
            pending_structured: HashMap::new(),
            structured_output: None,
        }
    }

    /// Emit the `system` init event (stream-json only).
    pub(crate) fn init(&mut self, info: InitInfo<'_>) -> std::io::Result<()> {
        if self.format != CliOutputFormat::StreamJson {
            return Ok(());
        }
        write_event(
            &mut self.out,
            &HeadlessEvent::System {
                subtype: "init",
                session_id: &self.session_id,
                model: info.model,
                cwd: info.cwd,
                tools: info.tools,
                permission_mode: info.permission_mode,
            },
        )
    }

    pub(crate) fn on_event(&mut self, event: QueryEvent) -> std::io::Result<()> {
        match event {
            QueryEvent::MessageCommitted(message) => self.on_message(&message),
            QueryEvent::ToolStart { tool_name, .. } => {
                if self.format == CliOutputFormat::Text {
                    writeln!(self.err, "[{}...]", tool_name)?;
                }
                Ok(())
            }
            QueryEvent::Error(msg) => writeln!(self.err, "Error: {}", msg),
            QueryEvent::Retry { attempt, max_attempts, delay_ms, reason } => match self.format {
                CliOutputFormat::StreamJson => write_event(
                    &mut self.out,
                    &HeadlessEvent::Retry {
                        attempt,
                        max_attempts,
                        delay_ms,
                        reason: &reason,
                        session_id: &self.session_id,
                    },
                ),
                _ => writeln!(
                    self.err,
                    "[Reconnecting {}/{} in {:.1}s: {}]",
                    attempt,
                    max_attempts,
                    delay_ms as f64 / 1000.0,
                    reason
                ),
            },
            QueryEvent::Compacted(record) => {
                if self.format == CliOutputFormat::StreamJson {
                    write_event(
                        &mut self.out,
                        &HeadlessEvent::Compacted { record: &record, session_id: &self.session_id },
                    )?;
                }
                self.compactions.push(record);
                Ok(())
            }
            QueryEvent::MaxTurnsReached { max_turns } => {
                self.max_turns = Some(max_turns);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn on_message(&mut self, message: &Message) -> std::io::Result<()> {
        self.track_structured_output(message);
        if message.role == Role::Assistant {
            self.num_turns += 1;
            self.last_assistant_text = message.get_all_text();
            if self.format == CliOutputFormat::Text && !self.last_assistant_text.is_empty() {
                if self.printed_text {
                    self.out.write_all(b"\n")?;
                }
                self.out.write_all(self.last_assistant_text.as_bytes())?;
                self.out.flush()?;
                self.printed_text = true;
            }
        }
        if self.format != CliOutputFormat::StreamJson {
            return Ok(());
        }
        let event_message = EventMessage::new(message);
        let session_id = self.session_id.as_str();
        let event = match message.role {
            Role::Assistant => HeadlessEvent::Assistant {
                message: event_message,
                parent_tool_use_id: None,
                session_id,
            },
            Role::User => HeadlessEvent::User {
                message: event_message,
                parent_tool_use_id: None,
                session_id,
            },
        };
        write_event(&mut self.out, &event)
    }

    /// Capture a StructuredOutput call's input once its tool result shows the
    /// tool accepted it (a schema mismatch comes back as an error result).
    fn track_structured_output(&mut self, message: &Message) {
        let MessageContent::Blocks(blocks) = &message.content else {
            return;
        };
        for block in blocks {
            match block {
                ContentBlock::ToolUse { id, name, input, .. }
                    if name == claurst_tools::SyntheticOutputTool::NAME =>
                {
                    self.pending_structured.insert(id.clone(), input.clone());
                }
                ContentBlock::ToolResult { tool_use_id, is_error, .. } => {
                    if let Some(input) = self.pending_structured.remove(tool_use_id) {
                        if *is_error != Some(true) {
                            self.structured_output = Some(input);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Build the result for `outcome`. Usage and cost are the run totals.
    pub(crate) fn result(
        &self,
        outcome: &QueryOutcome,
        cost_tracker: &claurst_core::cost::CostTracker,
        duration: std::time::Duration,
    ) -> RunResult {
        let (subtype, stop_reason, error) = classify(outcome, self.max_turns);
        RunResult {
            subtype,
            is_error: subtype != ResultSubtype::Success,
            result: self.last_assistant_text.clone(),
            stop_reason,
            error,
            session_id: self.session_id.clone(),
            num_turns: self.num_turns,
            duration_ms: duration.as_millis() as u64,
            total_cost_usd: cost_tracker.total_cost_usd(),
            usage: ResultUsage {
                input_tokens: cost_tracker.input_tokens(),
                output_tokens: cost_tracker.output_tokens(),
                cache_creation_input_tokens: cost_tracker.cache_creation_tokens(),
                cache_read_input_tokens: cost_tracker.cache_read_tokens(),
            },
            structured_output: self.structured_output.clone(),
        }
    }

    /// Write the end of the run: the `result` event (stream-json), the result
    /// object (json), or a closing newline plus any stop reason on stderr
    /// (text).
    pub(crate) fn finish(&mut self, result: &RunResult) -> std::io::Result<()> {
        match self.format {
            CliOutputFormat::StreamJson | CliOutputFormat::Json => {
                write_event(&mut self.out, &HeadlessEvent::Result(result))
            }
            CliOutputFormat::Text => {
                if self.printed_text {
                    self.out.write_all(b"\n")?;
                    self.out.flush()?;
                }
                match (&result.error, result.subtype) {
                    (Some(error), _) => writeln!(self.err, "Error: {}", error),
                    (None, ResultSubtype::Success) => Ok(()),
                    (None, _) => writeln!(self.err, "Stopped: {}", result.stop_reason),
                }
            }
        }
    }

    pub(crate) fn take_compactions(&mut self) -> Vec<CompactionRecord> {
        std::mem::take(&mut self.compactions)
    }
}

fn write_event<W: Write>(out: &mut W, event: &HeadlessEvent<'_>) -> std::io::Result<()> {
    serde_json::to_writer(&mut *out, event)?;
    out.write_all(b"\n")?;
    out.flush()
}

/// Map a loop outcome to the result subtype, stop reason and error text.
/// `max_turns` is set when the loop reported reaching its turn cap; the loop
/// still ends with `EndTurn` then (after the tool-less summary turn).
fn classify(outcome: &QueryOutcome, max_turns: Option<u32>) -> (ResultSubtype, &'static str, Option<String>) {
    match outcome {
        QueryOutcome::EndTurn { .. } => match max_turns {
            Some(max) => (
                ResultSubtype::ErrorMaxTurns,
                "max_turns",
                Some(format!("reached the maximum number of turns ({})", max)),
            ),
            None => (ResultSubtype::Success, "end_turn", None),
        },
        QueryOutcome::MaxTokens { .. } => (
            ResultSubtype::ErrorMaxTokens,
            "max_tokens",
            Some("the response hit the output-token limit".to_string()),
        ),
        QueryOutcome::BudgetExceeded { cost_usd, limit_usd } => (
            ResultSubtype::ErrorMaxBudgetUsd,
            "budget",
            Some(format!("budget limit ${:.4} reached (spent ${:.4})", limit_usd, cost_usd)),
        ),
        QueryOutcome::Cancelled => (ResultSubtype::Cancelled, "cancelled", None),
        QueryOutcome::Error(e) => (ResultSubtype::ErrorDuringExecution, "error", Some(e.to_string())),
    }
}

/// Build the StructuredOutput tool from `--json-schema`: inline JSON when the
/// value starts with `{`, otherwise the path of a JSON file.
pub(crate) fn structured_output_tool(raw: &str) -> anyhow::Result<claurst_tools::SyntheticOutputTool> {
    use anyhow::Context;

    let text = if raw.trim_start().starts_with('{') {
        Cow::Borrowed(raw)
    } else {
        Cow::Owned(std::fs::read_to_string(raw).with_context(|| format!("could not read {}", raw))?)
    };
    let schema: serde_json::Value =
        serde_json::from_str(&text).context("the schema is not valid JSON")?;
    claurst_tools::SyntheticOutputTool::new(schema).map_err(anyhow::Error::msg)
}

/// Which saved session a headless run continues.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SessionTarget<'a> {
    /// A new session with this id.
    Fresh(&'a str),
    /// The most recently updated session (`-c` / `--resume` without an id).
    Latest,
    /// This existing session (`--resume <id>`).
    Resume(&'a str),
    /// This session if it exists, else a new one with that id (`--session-id`).
    Named(&'a str),
}

/// Session ids name files under the sessions directory: no separators or
/// leading dots.
fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Pick the session from `--resume [id]`, `-c` and `--session-id`.
/// `fresh_id` names the session when none of them is given.
pub(crate) fn session_target<'a>(
    resume: Option<&'a str>,
    continue_session: bool,
    session_id: Option<&'a str>,
    fresh_id: &'a str,
) -> anyhow::Result<SessionTarget<'a>> {
    let target = match (resume, continue_session, session_id) {
        (Some(_), _, Some(_)) | (None, true, Some(_)) => {
            anyhow::bail!("--session-id cannot be combined with --resume or --continue")
        }
        (Some("__last__"), _, None) | (None, true, None) => SessionTarget::Latest,
        (Some(id), _, None) => SessionTarget::Resume(id),
        (None, false, Some(id)) => SessionTarget::Named(id),
        (None, false, None) => SessionTarget::Fresh(fresh_id),
    };
    if let SessionTarget::Resume(id) | SessionTarget::Named(id) = target {
        if !valid_session_id(id) {
            anyhow::bail!("invalid session id {:?}: use letters, digits, '-', '_' or '.'", id);
        }
    }
    Ok(target)
}

/// Load or create the session for `target`. A new session records `model`
/// and `cwd`. With `Latest` and no saved session, a new one is started and a
/// notice goes to stderr.
pub(crate) async fn open_session(
    target: SessionTarget<'_>,
    model: &str,
    cwd: &str,
) -> anyhow::Result<claurst_core::history::ConversationSession> {
    use claurst_core::history;

    let fresh = |id: &str| {
        let mut session = history::ConversationSession::new(model.to_string());
        session.id = id.to_string();
        session.working_dir = Some(cwd.to_string());
        session
    };
    match target {
        SessionTarget::Fresh(id) => Ok(fresh(id)),
        SessionTarget::Latest => match history::list_sessions().await.into_iter().next() {
            Some(session) => Ok(session),
            None => {
                eprintln!("No previous session found; starting a new session.");
                Ok(fresh(&uuid::Uuid::new_v4().to_string()))
            }
        },
        SessionTarget::Resume(id) => history::load_session(id)
            .await
            .map_err(|e| anyhow::anyhow!("could not load session {}: {}", id, e)),
        SessionTarget::Named(id) => match history::load_session(id).await {
            Ok(session) => Ok(session),
            Err(e)
                if e.downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
            {
                Ok(fresh(id))
            }
            Err(e) => Err(anyhow::anyhow!("could not load session {}: {}", id, e)),
        },
    }
}

/// Copy the finished run into `session` and save it with the same API the
/// interactive REPL uses. A failed save is reported on stderr.
pub(crate) async fn save_session(
    session: &mut claurst_core::history::ConversationSession,
    messages: Vec<Message>,
    compactions: Vec<CompactionRecord>,
    model: &str,
    cost_tracker: &claurst_core::cost::CostTracker,
) {
    session.messages = messages;
    session.compactions.extend(compactions);
    session.model = model.to_string();
    session.updated_at = chrono::Utc::now();
    session.total_cost += cost_tracker.total_cost_usd();
    session.total_tokens += cost_tracker.total_tokens();
    if let Err(e) = claurst_core::history::save_session(session).await {
        eprintln!("Warning: could not save session {}: {}", session.id, e);
    }
}

/// Run the query loop to completion, feeding every event to `sink`.
/// Returns the outcome and the final history.
pub(crate) async fn drive<W: Write, E: Write>(
    client: Arc<claurst_api::AnthropicClient>,
    tools: Arc<Vec<Box<dyn claurst_tools::Tool>>>,
    tool_ctx: claurst_tools::ToolContext,
    query_config: claurst_query::QueryConfig,
    cost_tracker: Arc<claurst_core::cost::CostTracker>,
    mut messages: Vec<Message>,
    cancel: tokio_util::sync::CancellationToken,
    sink: &mut HeadlessSink<W, E>,
) -> std::io::Result<(QueryOutcome, Vec<Message>)> {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<QueryEvent>();
    let loop_cancel = cancel.clone();
    let handle = tokio::spawn(async move {
        let outcome = claurst_query::run_query_loop(
            client.as_ref(),
            &mut messages,
            tools.as_slice(),
            &tool_ctx,
            &query_config,
            cost_tracker,
            Some(event_tx),
            loop_cancel,
            None,
        )
        .await;
        (outcome, messages)
    });

    while let Some(event) = event_rx.recv().await {
        if let Err(e) = sink.on_event(event) {
            // stdout is gone (e.g. a closed pipe): stop the run.
            cancel.cancel();
            return Err(e);
        }
    }

    Ok(handle.await.unwrap_or_else(|_| {
        (
            QueryOutcome::Error(claurst_core::error::ClaudeError::Other(
                "Query task panicked".to_string(),
            )),
            Vec::new(),
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use claurst_api::provider_types::StopReason;
    use claurst_api::{ProviderError, StreamEvent};
    use claurst_core::types::UsageInfo;
    use claurst_tools::{PermissionLevel, Tool, ToolContext, ToolResult};
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;

    type Events = Vec<Result<StreamEvent, ProviderError>>;

    /// One scripted provider response: a request-level error or a stream.
    enum Step {
        Fail(ProviderError),
        Stream(Events),
    }

    /// Replays `Step`s in order, then answers `end_turn` with "final answer".
    struct ScriptedProvider {
        id: claurst_core::provider_id::ProviderId,
        steps: StdMutex<VecDeque<Step>>,
    }

    fn pid() -> claurst_core::provider_id::ProviderId {
        claurst_core::provider_id::ProviderId::new("scriptprov")
    }

    fn text_turn(text: &str) -> Events {
        vec![
            Ok(StreamEvent::MessageStart {
                id: "msg-text".to_string(),
                model: "mock-model".to_string(),
                usage: UsageInfo::default(),
            }),
            Ok(StreamEvent::TextDelta { index: 0, text: text.to_string() }),
            Ok(StreamEvent::MessageDelta {
                stop_reason: Some(StopReason::EndTurn),
                usage: Some(UsageInfo::default()),
            }),
            Ok(StreamEvent::MessageStop),
        ]
    }

    fn tool_turn(call_id: &str, name: &str, args: &str) -> Events {
        vec![
            Ok(StreamEvent::MessageStart {
                id: format!("msg-{call_id}"),
                model: "mock-model".to_string(),
                usage: UsageInfo::default(),
            }),
            Ok(StreamEvent::ContentBlockStart {
                index: 0,
                content_block: ContentBlock::ToolUse {
                    id: call_id.to_string(),
                    name: name.to_string(),
                    input: serde_json::json!({}),
                    thought_signature: None,
                },
            }),
            Ok(StreamEvent::InputJsonDelta { index: 0, partial_json: args.to_string() }),
            Ok(StreamEvent::ContentBlockStop { index: 0 }),
            Ok(StreamEvent::MessageDelta {
                stop_reason: Some(StopReason::ToolUse),
                usage: Some(UsageInfo::default()),
            }),
            Ok(StreamEvent::MessageStop),
        ]
    }

    #[async_trait::async_trait]
    impl claurst_api::LlmProvider for ScriptedProvider {
        fn id(&self) -> &claurst_core::provider_id::ProviderId {
            &self.id
        }
        fn name(&self) -> &str {
            "scripted-mock"
        }

        async fn create_message(
            &self,
            _request: claurst_api::ProviderRequest,
        ) -> Result<claurst_api::ProviderResponse, ProviderError> {
            unimplemented!("these tests only use create_message_stream")
        }

        async fn create_message_stream(
            &self,
            _request: claurst_api::ProviderRequest,
        ) -> Result<
            std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamEvent, ProviderError>> + Send>>,
            ProviderError,
        > {
            let step = self.steps.lock().unwrap().pop_front();
            let events = match step {
                Some(Step::Fail(e)) => return Err(e),
                Some(Step::Stream(events)) => events,
                None => text_turn("final answer"),
            };
            Ok(Box::pin(futures::stream::iter(events)))
        }

        async fn health_check(&self) -> Result<claurst_api::ProviderStatus, ProviderError> {
            Ok(claurst_api::ProviderStatus::Healthy)
        }

        fn capabilities(&self) -> claurst_api::ProviderCapabilities {
            claurst_api::ProviderCapabilities {
                streaming: true,
                tool_calling: true,
                thinking: false,
                image_input: false,
                pdf_input: false,
                audio_input: false,
                video_input: false,
                caching: false,
                structured_output: false,
                system_prompt_style: claurst_api::SystemPromptStyle::TopLevel,
            }
        }
    }

    /// A read-only tool that answers "pong".
    struct PingTool;

    #[async_trait::async_trait]
    impl Tool for PingTool {
        fn name(&self) -> &str {
            "ping"
        }
        fn description(&self) -> &str {
            "Answers pong."
        }
        fn permission_level(&self) -> PermissionLevel {
            PermissionLevel::ReadOnly
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        async fn execute(&self, _input: serde_json::Value, _ctx: &ToolContext) -> ToolResult {
            ToolResult::success("pong")
        }
    }

    fn context(
        working_dir: std::path::PathBuf,
        handler: Arc<dyn claurst_core::PermissionHandler>,
    ) -> ToolContext {
        let config = claurst_core::config::Config {
            provider: Some("scriptprov".to_string()),
            ..Default::default()
        };
        ToolContext {
            working_dir,
            permission_mode: claurst_core::config::PermissionMode::Default,
            permission_handler: handler,
            cost_tracker: claurst_core::cost::CostTracker::new(),
            session_id: "headless-test".to_string(),
            file_history: Arc::new(parking_lot::Mutex::new(
                claurst_core::file_history::FileHistory::new(),
            )),
            current_turn: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            non_interactive: true,
            mcp_manager: None,
            config,
            managed_agent_config: None,
            completion_notifier: None,
            pending_permissions: None,
            permission_manager: None,
            user_question_tx: None,
            cancel_token: tokio_util::sync::CancellationToken::new(),
            batch_rejection: Default::default(),
        }
    }

    fn allow_all() -> Arc<dyn claurst_core::PermissionHandler> {
        Arc::new(claurst_core::permissions::AutoPermissionHandler {
            mode: claurst_core::config::PermissionMode::BypassPermissions,
        })
    }

    fn scripted_config(steps: Vec<Step>) -> claurst_query::QueryConfig {
        let provider = Arc::new(ScriptedProvider { id: pid(), steps: StdMutex::new(steps.into()) });
        let mut registry = claurst_api::ProviderRegistry::new();
        registry.register(provider);
        claurst_query::QueryConfig {
            model: "mock-model".to_string(),
            provider_registry: Some(Arc::new(registry)),
            retry: claurst_api::RetryConfig {
                max_retries: 3,
                initial_delay: std::time::Duration::from_millis(1),
                ..claurst_api::RetryConfig::default()
            },
            auto_compact: false,
            ..claurst_query::QueryConfig::default()
        }
    }

    struct Run {
        stdout: String,
        stderr: String,
        outcome: QueryOutcome,
        messages: Vec<Message>,
        result: RunResult,
    }

    async fn run_with(
        format: CliOutputFormat,
        config: claurst_query::QueryConfig,
        tools: Vec<Box<dyn Tool>>,
        ctx: ToolContext,
        history: Vec<Message>,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Run {
        let client = Arc::new(
            claurst_api::AnthropicClient::new(claurst_api::client::ClientConfig {
                api_key: "test-key".to_string(),
                ..Default::default()
            })
            .expect("build test client"),
        );
        let tracker = claurst_core::cost::CostTracker::new();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (outcome, messages, result) = {
            let mut sink = HeadlessSink::new(format, ctx.session_id.clone(), &mut out, &mut err);
            let tool_names: Vec<String> = tools.iter().map(|t| t.name().to_string()).collect();
            sink.init(InitInfo {
                model: &config.model,
                cwd: "/workspace",
                tools: tool_names.iter().map(String::as_str).collect(),
                permission_mode: &ctx.permission_mode,
            })
            .unwrap();
            let (outcome, messages) = tokio::time::timeout(
                std::time::Duration::from_secs(20),
                drive(client, Arc::new(tools), ctx, config, tracker.clone(), history, cancel, &mut sink),
            )
            .await
            .expect("headless run must not hang")
            .unwrap();
            let result = sink.result(&outcome, &tracker, std::time::Duration::from_millis(5));
            sink.finish(&result).unwrap();
            (outcome, messages, result)
        };
        Run {
            stdout: String::from_utf8(out).unwrap(),
            stderr: String::from_utf8(err).unwrap(),
            outcome,
            messages,
            result,
        }
    }

    async fn run(format: CliOutputFormat, steps: Vec<Step>, tools: Vec<Box<dyn Tool>>) -> Run {
        run_with(
            format,
            scripted_config(steps),
            tools,
            context(std::path::PathBuf::from("/workspace"), allow_all()),
            vec![Message::user("start")],
            tokio_util::sync::CancellationToken::new(),
        )
        .await
    }

    fn lines(stdout: &str) -> Vec<serde_json::Value> {
        stdout
            .lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
            .collect()
    }

    fn kinds(events: &[serde_json::Value]) -> Vec<&str> {
        events.iter().map(|e| e["type"].as_str().unwrap()).collect()
    }

    #[tokio::test]
    async fn stream_json_event_shape_and_order() {
        let run = run(
            CliOutputFormat::StreamJson,
            vec![Step::Stream(tool_turn("call-1", "ping", "{}"))],
            vec![Box::new(PingTool)],
        )
        .await;
        assert!(matches!(run.outcome, QueryOutcome::EndTurn { .. }));
        let events = lines(&run.stdout);
        assert_eq!(kinds(&events), ["system", "assistant", "user", "assistant", "result"], "{}", run.stdout);

        let init = &events[0];
        assert_eq!(init["subtype"], "init");
        assert_eq!(init["model"], "mock-model");
        assert_eq!(init["cwd"], "/workspace");
        assert_eq!(init["tools"], serde_json::json!(["ping"]));
        assert_eq!(init["permissionMode"], "default");

        let tool_use = &events[1]["message"];
        assert_eq!(tool_use["role"], "assistant");
        assert_eq!(tool_use["id"], "msg-call-1");
        assert_eq!(tool_use["content"][0]["type"], "tool_use");
        assert_eq!(tool_use["content"][0]["id"], "call-1");
        assert_eq!(tool_use["content"][0]["name"], "ping");

        let tool_result = &events[2]["message"];
        assert_eq!(tool_result["role"], "user");
        assert_eq!(tool_result["content"][0]["type"], "tool_result");
        assert_eq!(tool_result["content"][0]["tool_use_id"], "call-1");
        assert_eq!(tool_result["content"][0]["content"], "pong");

        assert_eq!(events[3]["message"]["content"][0]["text"], "final answer");

        let result = &events[4];
        assert_eq!(result["subtype"], "success");
        assert_eq!(result["is_error"], false);
        assert_eq!(result["result"], "final answer");
        assert_eq!(result["stop_reason"], "end_turn");
        assert_eq!(result["num_turns"], 2);
        for key in ["duration_ms", "total_cost_usd", "usage"] {
            assert!(result.get(key).is_some(), "result lacks {key}: {result}");
        }
        assert!(events.iter().all(|e| e["session_id"] == "headless-test"), "{}", run.stdout);
        for message_event in &events[1..4] {
            assert_eq!(
                message_event.get("parent_tool_use_id"),
                Some(&serde_json::Value::Null),
                "parent_tool_use_id is present and null: {message_event}"
            );
        }
        assert!(run.stderr.is_empty(), "stream-json keeps narration off stderr: {}", run.stderr);
    }

    #[tokio::test]
    async fn json_prints_only_the_result_object() {
        let run = run(
            CliOutputFormat::Json,
            vec![Step::Stream(tool_turn("call-1", "ping", "{}"))],
            vec![Box::new(PingTool)],
        )
        .await;
        let events = lines(&run.stdout);
        assert_eq!(kinds(&events), ["result"], "{}", run.stdout);
        assert_eq!(events[0]["result"], "final answer", "the last message, not all turns joined");
    }

    #[tokio::test]
    async fn stream_json_reports_retries() {
        let error = claurst_api::parse_error_response(503, "upstream unavailable", &pid());
        let run = run(CliOutputFormat::StreamJson, vec![Step::Fail(error)], Vec::new()).await;
        let events = lines(&run.stdout);
        assert_eq!(kinds(&events), ["system", "retry", "assistant", "result"], "{}", run.stdout);
        assert_eq!(events[1]["attempt"], 1);
        assert_eq!(events[1]["max_attempts"], 3);
    }

    #[tokio::test]
    async fn text_mode_prints_a_retried_turn_once() {
        let mut cut = text_turn("final ans");
        cut.truncate(2);
        cut.push(Err(ProviderError::StreamError {
            provider: pid(),
            message: "Stream read error: connection reset".to_string(),
            partial_response: None,
        }));
        let run = run(CliOutputFormat::Text, vec![Step::Stream(cut)], Vec::new()).await;
        assert!(matches!(run.outcome, QueryOutcome::EndTurn { .. }));
        assert_eq!(run.stdout, "final answer\n", "the failed attempt's text must not be printed");
        assert!(run.stderr.contains("Reconnecting 1/3"), "{}", run.stderr);
        assert_eq!(run.result.num_turns, 1);
        assert_eq!(run.messages.len(), 2, "the user prompt plus one assistant reply");
    }

    #[tokio::test]
    async fn end_turn_exits_0() {
        let run = run(CliOutputFormat::Json, Vec::new(), Vec::new()).await;
        assert_eq!(run.result.subtype, ResultSubtype::Success);
        assert_eq!(run.result.subtype.exit_code(), 0);
    }

    #[tokio::test]
    async fn error_exits_1_with_the_error_in_the_result() {
        let error = claurst_api::parse_error_response(401, "invalid x-api-key", &pid());
        let run = run(CliOutputFormat::Json, vec![Step::Fail(error)], Vec::new()).await;
        let events = lines(&run.stdout);
        assert_eq!(events[0]["subtype"], "error_during_execution");
        assert_eq!(events[0]["is_error"], true);
        assert_eq!(events[0]["stop_reason"], "error");
        assert!(events[0]["error"].as_str().is_some(), "{}", run.stdout);
        assert_eq!(run.result.subtype.exit_code(), 1);
    }

    #[tokio::test]
    async fn max_turns_exits_2_after_the_summary_turn() {
        let mut config = scripted_config(vec![Step::Stream(tool_turn("call-1", "ping", "{}"))]);
        config.max_turns = 1;
        let run = run_with(
            CliOutputFormat::StreamJson,
            config,
            vec![Box::new(PingTool)],
            context(std::path::PathBuf::from("/workspace"), allow_all()),
            vec![Message::user("start")],
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        assert!(matches!(run.outcome, QueryOutcome::EndTurn { .. }));
        let events = lines(&run.stdout);
        let result = events.last().unwrap();
        assert_eq!(result["subtype"], "error_max_turns", "{}", run.stdout);
        assert_eq!(result["stop_reason"], "max_turns");
        assert_eq!(result["result"], "final answer", "the summary turn's text is the result");
        assert_eq!(run.result.subtype.exit_code(), 2);
    }

    #[tokio::test]
    async fn cancel_exits_130_with_a_cancelled_result() {
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let run = run_with(
            CliOutputFormat::Json,
            scripted_config(Vec::new()),
            Vec::new(),
            context(std::path::PathBuf::from("/workspace"), allow_all()),
            vec![Message::user("start")],
            cancel,
        )
        .await;
        assert!(matches!(run.outcome, QueryOutcome::Cancelled));
        assert_eq!(lines(&run.stdout)[0]["subtype"], "cancelled");
        assert_eq!(run.result.subtype.exit_code(), 130);
    }

    #[test]
    fn budget_and_max_tokens_exit_2_with_a_reason() {
        let budget = QueryOutcome::BudgetExceeded { cost_usd: 1.5, limit_usd: 1.0 };
        let (subtype, stop_reason, error) = classify(&budget, None);
        assert_eq!((subtype, stop_reason), (ResultSubtype::ErrorMaxBudgetUsd, "budget"));
        assert!(error.unwrap().contains("$1.0000"));
        assert_eq!(subtype.exit_code(), 2);

        let max_tokens = QueryOutcome::MaxTokens {
            partial_message: Message::assistant("cut"),
            usage: UsageInfo::default(),
        };
        let (subtype, stop_reason, error) = classify(&max_tokens, None);
        assert_eq!((subtype, stop_reason), (ResultSubtype::ErrorMaxTokens, "max_tokens"));
        assert!(error.is_some());
        assert_eq!(subtype.exit_code(), 2);
    }

    #[test]
    fn text_mode_reports_a_limit_on_stderr() {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut sink = HeadlessSink::new(CliOutputFormat::Text, "s".to_string(), &mut out, &mut err);
        let outcome = QueryOutcome::BudgetExceeded { cost_usd: 1.5, limit_usd: 1.0 };
        let result = sink.result(&outcome, &claurst_core::cost::CostTracker::new(), std::time::Duration::ZERO);
        sink.finish(&result).unwrap();
        drop(sink);
        assert!(out.is_empty());
        assert!(String::from_utf8(err).unwrap().contains("budget limit $1.0000 reached"));
    }

    #[test]
    fn session_flags_pick_the_target() {
        fn t<'a>(
            resume: Option<&'a str>,
            cont: bool,
            id: Option<&'a str>,
        ) -> anyhow::Result<SessionTarget<'a>> {
            session_target(resume, cont, id, "fresh")
        }
        assert_eq!(t(None, false, None).unwrap(), SessionTarget::Fresh("fresh"));
        assert_eq!(t(None, true, None).unwrap(), SessionTarget::Latest);
        assert_eq!(t(Some("__last__"), false, None).unwrap(), SessionTarget::Latest);
        assert_eq!(t(Some("abc-1"), false, None).unwrap(), SessionTarget::Resume("abc-1"));
        assert_eq!(t(None, false, Some("abc-1")).unwrap(), SessionTarget::Named("abc-1"));
        assert!(t(Some("abc"), false, Some("abc")).is_err(), "--resume with --session-id");
        assert!(t(None, true, Some("abc")).is_err(), "-c with --session-id");
        assert!(t(Some("../escape"), false, None).is_err(), "path separators are refused");
        assert!(t(None, false, Some(".hidden")).is_err());
    }

    /// Append `prompt` to the session's history, run it and save the session.
    async fn run_and_save(
        session: &mut claurst_core::history::ConversationSession,
        prompt: &str,
        reply: &str,
    ) -> Run {
        let mut history = std::mem::take(&mut session.messages);
        history.push(Message::user(prompt));
        let mut ctx = context(std::path::PathBuf::from("/workspace"), allow_all());
        ctx.session_id = session.id.clone();
        let run = run_with(
            CliOutputFormat::Json,
            scripted_config(vec![Step::Stream(text_turn(reply))]),
            Vec::new(),
            ctx,
            history,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
        let tracker = claurst_core::cost::CostTracker::new();
        save_session(session, run.messages.clone(), Vec::new(), "mock-model", &tracker).await;
        run
    }

    #[tokio::test]
    async fn resume_round_trip_continues_the_saved_history() {
        let home = std::env::temp_dir().join(format!("claurst-headless-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let previous = std::env::var_os("CLAURST_HOME");
        std::env::set_var("CLAURST_HOME", &home);

        // --session-id creates the session when it does not exist yet.
        let target = session_target(None, false, Some("rt-1"), "unused").unwrap();
        let mut session = open_session(target, "mock-model", "/workspace").await.unwrap();
        assert_eq!(session.id, "rt-1");
        assert!(session.messages.is_empty());
        let first = run_and_save(&mut session, "first prompt", "one").await;
        assert_eq!(first.result.session_id, "rt-1");

        // --resume <id> continues it.
        let target = session_target(Some("rt-1"), false, None, "unused").unwrap();
        let mut session = open_session(target, "mock-model", "/workspace").await.unwrap();
        assert_eq!(session.messages.len(), 2, "the first run was saved");
        let second = run_and_save(&mut session, "second prompt", "two").await;
        assert_eq!(second.result.result, "two");
        let texts: Vec<String> = second.messages.iter().map(Message::get_all_text).collect();
        assert_eq!(texts, ["first prompt", "one", "second prompt", "two"]);

        // -c picks the most recent session; a missing --resume id is an error.
        let target = session_target(None, true, None, "unused").unwrap();
        let latest = open_session(target, "mock-model", "/workspace").await.unwrap();
        assert_eq!(latest.id, "rt-1");
        assert_eq!(latest.messages.len(), 4);
        let missing = session_target(Some("no-such-session"), false, None, "unused").unwrap();
        assert!(open_session(missing, "mock-model", "/workspace").await.is_err());

        match previous {
            Some(value) => std::env::set_var("CLAURST_HOME", value),
            None => std::env::remove_var("CLAURST_HOME"),
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn permission_ask_is_rejected_with_a_reason_and_the_run_continues() {
        let workspace = std::env::temp_dir().join(format!("claurst-headless-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let target = workspace.join("out.txt");
        let manager = claurst_core::permissions::PermissionManager::new(
            claurst_core::config::PermissionMode::Default,
            &claurst_core::config::Settings::default(),
        );
        let handler = claurst_core::permissions::AutoPermissionHandler::with_manager(Arc::new(
            std::sync::Mutex::new(manager),
        ));
        let args = serde_json::json!({"file_path": target.display().to_string(), "content": "x"});
        let run = run_with(
            CliOutputFormat::StreamJson,
            scripted_config(vec![Step::Stream(tool_turn("call-w", "Write", &args.to_string()))]),
            vec![Box::new(claurst_tools::FileWriteTool)],
            context(workspace.clone(), Arc::new(handler)),
            vec![Message::user("start")],
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

        assert!(!target.exists(), "the rejected write must not run");
        let events = lines(&run.stdout);
        let rejection = &events[2]["message"]["content"][0];
        assert_eq!(rejection["tool_use_id"], "call-w");
        assert_eq!(rejection["is_error"], true);
        let text = rejection["content"].as_str().unwrap();
        assert!(text.contains("Permission denied for tool 'Write'"), "{text}");
        assert!(text.contains("non-interactive"), "the model is told why: {text}");
        assert_eq!(events.last().unwrap()["subtype"], "success", "the run continues to its end");
        assert_eq!(run.result.result, "final answer");
        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn json_schema_output_is_captured_in_the_result() {
        let schema = r#"{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"]}"#;
        let tool = structured_output_tool(schema).unwrap();
        let run = run(
            CliOutputFormat::StreamJson,
            vec![
                // Rejected by the schema: not captured, the model retries.
                Step::Stream(tool_turn("call-bad", "StructuredOutput", r#"{"answer":42}"#)),
                Step::Stream(tool_turn("call-ok", "StructuredOutput", r#"{"answer":"42"}"#)),
            ],
            vec![Box::new(tool)],
        )
        .await;
        let events = lines(&run.stdout);
        assert_eq!(events[2]["message"]["content"][0]["is_error"], true, "{}", run.stdout);
        let result = events.last().unwrap();
        assert_eq!(result["subtype"], "success");
        assert_eq!(result["structured_output"], serde_json::json!({"answer": "42"}));
    }

    #[tokio::test]
    async fn without_json_schema_the_result_has_no_structured_output() {
        let run = run(CliOutputFormat::Json, Vec::new(), Vec::new()).await;
        assert!(lines(&run.stdout)[0].get("structured_output").is_none());
    }

    #[test]
    fn json_schema_accepts_inline_json_or_a_file_and_needs_an_object_schema() {
        assert!(structured_output_tool(r#"{"type":"object"}"#).is_ok());
        assert!(structured_output_tool(r#"{"type":"array"}"#).is_err());
        assert!(structured_output_tool("{not json").is_err());

        let path = std::env::temp_dir().join(format!("claurst-schema-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&path, r#"{"type":"object","properties":{}}"#).unwrap();
        let tool = structured_output_tool(&path.display().to_string()).unwrap();
        assert_eq!(claurst_tools::Tool::name(&tool), "StructuredOutput");
        let _ = std::fs::remove_file(&path);
        assert!(structured_output_tool("/no/such/schema.json").is_err());
    }

    #[test]
    fn structured_output_is_not_a_default_tool() {
        assert!(claurst_tools::all_tools().iter().all(|t| t.name() != "StructuredOutput"));
    }
}
