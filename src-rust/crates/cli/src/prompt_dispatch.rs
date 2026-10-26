//! Local prompt actions handled before steering or submitting an API query.

/// A true result means the live CLI must not send this input to the model.
pub(crate) fn try_bang(app: &mut claurst_tui::App, input: &str) -> bool {
    if !app.config.bang_commands.enabled {
        return false;
    }
    let Some(command) = input.strip_prefix('!') else {
        return false;
    };
    if command.trim().is_empty() {
        app.status_message = Some("Usage: !command".into());
    } else {
        app.execute_bang_command(command.to_owned());
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use claurst_core::{Config, PermissionMode};

    fn app(enabled: bool) -> claurst_tui::App {
        let mut config = Config::default();
        config.bang_commands.enabled = enabled;
        config.project_dir = Some(std::env::temp_dir());
        claurst_tui::App::new(config, claurst_core::cost::CostTracker::new())
    }

    #[tokio::test]
    async fn live_prompt_route_executes_bang_without_a_provider() {
        let mut app = app(true);
        assert!(try_bang(&mut app, "!echo reviewer-smoke"));
        assert!(app.bang_command_pending.is_some());
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                app.poll_session_loads();
                if app.bang_command_pending.is_none() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("local shell completion must reach the live transcript");
        assert!(app
            .system_annotations
            .iter()
            .any(|item| item.text.contains("reviewer-smoke")));
        assert!(
            app.messages.is_empty(),
            "bang input must not become a model prompt"
        );
    }

    #[test]
    fn disabled_bang_keeps_literal_prompt_behavior() {
        let mut app = app(false);
        assert!(!try_bang(&mut app, "!literal text"));
        assert!(app.bang_command_pending.is_none());
    }

    #[test]
    fn plan_mode_consumes_bang_without_starting_shell_or_query() {
        let mut app = app(true);
        app.config.permission_mode = PermissionMode::Plan;
        assert!(try_bang(&mut app, "!echo blocked"));
        assert!(app.bang_command_pending.is_none());
        assert!(!app.notifications.is_empty());
        assert!(app.messages.is_empty());
    }
}
