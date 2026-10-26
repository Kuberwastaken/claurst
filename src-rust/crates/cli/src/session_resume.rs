//! One idle-session transition for `/resume` and the session browser.

use std::path::Path;
use std::sync::Arc;

use claurst_commands::CommandContext;
use claurst_core::history::ConversationSession;
use claurst_core::session_journal::SessionJournal;
use claurst_core::types::Message;
use claurst_query::QueryConfig;
use claurst_tools::ToolContext;
use claurst_tui::App;
use parking_lot::Mutex;

/// Call only after the previous query has drained. Acquire the destination
/// before changing any live state, then bind every writer to that session.
pub(crate) fn apply(
    resumed: ConversationSession,
    sessions_dir: &Path,
    session: &mut ConversationSession,
    journal: &mut Arc<SessionJournal>,
    query: &mut QueryConfig,
    messages: &mut Vec<Message>,
    app: &mut App,
    commands: &mut CommandContext,
    tools: &mut ToolContext,
) -> anyhow::Result<()> {
    let destination = if resumed.id == session.id {
        journal.clone()
    } else {
        SessionJournal::new(sessions_dir, &resumed)
    };
    destination.acquire()?;
    session.messages = messages.clone();
    journal.checkpoint(session)?;

    *session = resumed;
    *journal = destination;
    query.session_journal = Some(journal.clone());
    query.model = session.model.clone();
    *messages = session.messages.clone();
    app.replace_messages(messages.clone());
    commands.messages = messages.clone();
    commands.config.model = Some(session.model.clone());
    app.config.model = Some(session.model.clone());
    tools.config.model = Some(session.model.clone());
    app.model_name = session.model.clone();
    tools.session_id = session.id.clone();
    tools.file_history = Arc::new(Mutex::new(claurst_core::file_history::FileHistory::new()));
    tools.current_turn = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    commands.session_id = session.id.clone();
    commands.session_title = session.title.clone();
    app.session_title = session.title.clone();
    if let Some(saved_dir) = session.working_dir.as_deref() {
        let saved_path = std::path::PathBuf::from(saved_dir);
        if saved_path.is_dir() {
            tools.working_dir = saved_path.clone();
            commands.working_dir = saved_path;
        }
    }
    app.config.project_dir = Some(tools.working_dir.clone());
    query.working_directory = Some(tools.working_dir.display().to_string());
    app.current_session_id = Some(session.id.clone());
    app.attach_turn_diff_state(tools.file_history.clone(), tools.current_turn.clone());
    claurst_tui::update_terminal_title(session.title.as_deref());
    app.status_message = Some(format!("Resumed session {}.", session.id));
    Ok(())
}

/// Persist a live rename through its owning journal before reporting success.
/// Used for both `/rename` and a browser rename of the current session.
pub(crate) fn rename_current(
    title: String,
    messages: &[Message],
    session: &mut ConversationSession,
    journal: &Arc<SessionJournal>,
    commands: &mut CommandContext,
    app: &mut App,
) -> anyhow::Result<()> {
    session.messages = messages.to_vec();
    session.title = Some(title.clone());
    session.updated_at = chrono::Utc::now();
    journal.checkpoint(session)?;
    commands.session_title = session.title.clone();
    app.session_title = session.title.clone();
    claurst_tui::update_terminal_title(Some(&title));
    app.status_message = Some(format!("Session renamed to \"{title}\"."));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use claurst_core::config::Config;
    use claurst_core::cost::CostTracker;
    use claurst_core::permissions::AutoPermissionHandler;

    struct Harness {
        session: ConversationSession,
        journal: Arc<SessionJournal>,
        query: QueryConfig,
        messages: Vec<Message>,
        app: App,
        commands: CommandContext,
        tools: ToolContext,
    }

    impl Harness {
        fn new(dir: &Path) -> Self {
            let mut session = ConversationSession::new("model-a".into());
            session.messages.push(Message::user("only A"));
            let journal = SessionJournal::new(dir, &session);
            journal.checkpoint(&mut session).unwrap();
            let config = Config::default();
            let cost = CostTracker::new();
            let tools = ToolContext {
                working_dir: dir.to_path_buf(),
                permission_mode: config.permission_mode.clone(),
                permission_handler: Arc::new(AutoPermissionHandler {
                    mode: config.permission_mode.clone(),
                }),
                cost_tracker: cost.clone(),
                session_id: session.id.clone(),
                file_history: Arc::new(Mutex::new(claurst_core::file_history::FileHistory::new())),
                current_turn: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                non_interactive: true,
                mcp_manager: None,
                config: config.clone(),
                managed_agent_config: None,
                completion_notifier: None,
                pending_permissions: None,
                permission_manager: None,
                user_question_tx: None,
                cancel_token: tokio_util::sync::CancellationToken::new(),
                batch_rejection: Default::default(),
            };
            let commands = CommandContext {
                config: config.clone(),
                cost_tracker: cost.clone(),
                messages: session.messages.clone(),
                working_dir: dir.to_path_buf(),
                session_id: session.id.clone(),
                session_title: None,
                remote_session_url: None,
                mcp_manager: None,
                mcp_auth_runner: None,
                permission_manager: None,
                disabled_mcp_servers: Default::default(),
            };
            let query = QueryConfig {
                session_journal: Some(journal.clone()),
                ..Default::default()
            };
            Self {
                messages: session.messages.clone(),
                session,
                journal,
                query,
                app: App::new(config, cost),
                commands,
                tools,
            }
        }

        fn resume(&mut self, resumed: ConversationSession, dir: &Path) -> anyhow::Result<()> {
            apply(
                resumed,
                dir,
                &mut self.session,
                &mut self.journal,
                &mut self.query,
                &mut self.messages,
                &mut self.app,
                &mut self.commands,
                &mut self.tools,
            )
        }
    }

    #[test]
    fn resumed_query_and_checkpoint_write_b_without_changing_a() {
        let dir = tempfile::tempdir().unwrap();
        let mut live = Harness::new(dir.path());
        let a = live.session.id.clone();
        let a_before = std::fs::read(dir.path().join(format!("{a}.json"))).unwrap();
        let mut b = ConversationSession::new("model-b".into());
        b.messages.push(Message::user("only B"));
        let b_id = b.id.clone();
        live.resume(b, dir.path()).unwrap();
        live.messages
            .push(Message::assistant("B after browser resume"));
        live.query
            .session_journal
            .as_ref()
            .unwrap()
            .sync_messages(&live.messages)
            .unwrap();
        live.session.messages = live.messages.clone();
        live.journal.checkpoint(&mut live.session).unwrap();
        let before: serde_json::Value = serde_json::from_slice(&a_before).unwrap();
        let after: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join(format!("{a}.json"))).unwrap())
                .unwrap();
        assert_eq!(after["messages"], before["messages"]);
        let a_loaded = claurst_core::session_journal::load(dir.path(), &a).unwrap();
        let b_loaded = claurst_core::session_journal::load(dir.path(), &b_id).unwrap();
        assert_eq!(a_loaded.session.messages.len(), 1);
        assert_eq!(b_loaded.session.messages.len(), 2);
        assert_eq!(live.tools.session_id, b_id);
        assert_eq!(live.commands.session_id, b_id);
        assert_eq!(live.app.current_session_id.as_deref(), Some(b_id.as_str()));
        assert!(Arc::ptr_eq(
            live.query.session_journal.as_ref().unwrap(),
            &live.journal
        ));
    }

    #[test]
    fn resuming_current_session_keeps_its_existing_lock() {
        let dir = tempfile::tempdir().unwrap();
        let mut live = Harness::new(dir.path());
        let existing = live.journal.clone();
        live.resume(live.session.clone(), dir.path()).unwrap();
        assert!(Arc::ptr_eq(&live.journal, &existing));
    }

    #[test]
    fn switching_checkpoints_idle_message_edits_before_leaving_a() {
        let dir = tempfile::tempdir().unwrap();
        let mut live = Harness::new(dir.path());
        let a = live.session.id.clone();
        live.messages.push(Message::user("idle edit"));
        live.resume(ConversationSession::new("model-b".into()), dir.path())
            .unwrap();
        let loaded = claurst_core::session_journal::load(dir.path(), &a).unwrap();
        assert_eq!(loaded.session.messages.len(), 2);
    }

    #[test]
    fn browser_rename_survives_the_next_live_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let mut live = Harness::new(dir.path());
        rename_current(
            "browser title".into(),
            &live.messages,
            &mut live.session,
            &live.journal,
            &mut live.commands,
            &mut live.app,
        )
        .unwrap();
        live.journal.checkpoint(&mut live.session).unwrap();
        let loaded = claurst_core::session_journal::load(dir.path(), &live.session.id).unwrap();
        assert_eq!(loaded.session.title.as_deref(), Some("browser title"));
        assert_eq!(
            live.commands.session_title.as_deref(),
            Some("browser title")
        );
        assert_eq!(live.app.session_title.as_deref(), Some("browser title"));
    }

    #[test]
    fn locked_destination_keeps_every_writer_on_a() {
        let dir = tempfile::tempdir().unwrap();
        let mut live = Harness::new(dir.path());
        let a = live.session.id.clone();
        let b = ConversationSession::new("model-b".into());
        let held = SessionJournal::new(dir.path(), &b);
        held.acquire().unwrap();
        assert!(live.resume(b, dir.path()).is_err());
        assert_eq!(live.session.id, a);
        assert_eq!(live.tools.session_id, a);
        assert_eq!(live.commands.session_id, a);
        assert!(Arc::ptr_eq(
            live.query.session_journal.as_ref().unwrap(),
            &live.journal
        ));
    }
}
