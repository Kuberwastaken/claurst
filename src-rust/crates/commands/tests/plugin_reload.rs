//! Exercise real plugin discovery/reload without sharing env or global registry
//! mutations with other tests. The child owns its temporary Claurst home.
use claurst_commands::{
    CommandContext, CommandResult, PluginCommand, ReloadPluginsCommand, SlashCommand,
};
use claurst_core::config::{Config, McpServerOrigin, Settings};
use serde_json::json;

#[test]
fn reload_refreshes_plugin_registry_and_trust_gated_roster() {
    let dir = tempfile::tempdir().unwrap();
    for bare in [false, true] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "reload_fixture_child", "--nocapture"])
            .env("CLAURST_PLUGIN_RELOAD_FIXTURE", dir.path())
            .env("CLAURST_HOME", dir.path().join("home"))
            .env("CLAURST_SIMPLE", if bare { "1" } else { "0" })
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[tokio::test]
async fn reload_fixture_child() {
    let Some(root) = std::env::var_os("CLAURST_PLUGIN_RELOAD_FIXTURE") else {
        return;
    };
    let root = std::path::PathBuf::from(root);
    let project = root.join("project");
    let home = Settings::config_dir();
    fn write_plugin(path: &std::path::Path, name: &str, server: &str) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(
            path.join("plugin.json"),
            json!({"name":name,"mcpServers":{server:{"command":"never-executed"}}}).to_string(),
        )
        .unwrap();
    }
    write_plugin(&home.join("plugins/user"), "user", "user-old");
    write_plugin(
        &project.join(".claurst/plugins/repo"),
        "repo",
        "repo-plugin",
    );
    let mut settings = Settings::default();
    settings.save_sync().unwrap();
    let initial = claurst_plugins::load_plugins(&project, &[]).await;
    if claurst_core::feature_gates::is_bare_mode() {
        assert!(initial.all_mcp_servers().is_empty());
        return;
    }
    claurst_plugins::set_global_registry(initial);
    let old_snapshot = claurst_plugins::global_plugin_registry().unwrap();
    assert!(old_snapshot
        .all_mcp_servers()
        .iter()
        .any(|s| s.name == "user-old"));
    write_plugin(&home.join("plugins/user"), "user", "user-new");
    let mut context = CommandContext {
        config: Config::default(),
        cost_tracker: claurst_core::cost::CostTracker::new(),
        messages: vec![],
        working_dir: project.clone(),
        session_id: "reload-fixture".into(),
        session_title: None,
        remote_session_url: None,
        mcp_manager: None,
        mcp_auth_runner: None,
        permission_manager: None,
        disabled_mcp_servers: Default::default(),
    };
    assert!(matches!(
        PluginCommand.execute("reload", &mut context).await,
        CommandResult::ReloadMcpRoster(_)
    ));
    let snapshot = claurst_plugins::global_plugin_registry().unwrap();
    let roster = snapshot.mcp_roster(&[]);
    assert!(roster.iter().any(|s| s.name == "user-new"));
    assert!(!roster.iter().any(|s| s.name == "user-old"));
    assert!(
        old_snapshot
            .all_mcp_servers()
            .iter()
            .any(|s| s.name == "user-old"),
        "in-flight readers keep their original snapshot"
    );
    assert_eq!(
        roster
            .iter()
            .find(|s| s.name == "repo-plugin")
            .unwrap()
            .origin,
        McpServerOrigin::Project
    );
    let decision = claurst_core::mcp_trust::partition_mcp_servers(
        &roster,
        Some(&project),
        false,
        &Default::default(),
        &Default::default(),
        &Default::default(),
    );
    assert_eq!(decision.allowed.len(), 1);
    assert_eq!(decision.allowed[0].name, "user-new");
    assert_eq!(decision.pending[0].name, "repo-plugin");

    // The import/rebuild path re-reads hierarchical settings rather than
    // projecting from a global-only config that lost its project entries.
    let project_settings = project.join(".claurst/settings.json");
    std::fs::write(project_settings, json!({"config":{"mcp_servers":[{"name":"project-settings","command":"never-executed","args":[],"env":{},"server_type":"stdio"}]}}).to_string()).unwrap();
    settings.config.mcp_servers = vec![serde_json::from_value(json!({"name":"imported-global","command":"never-executed","args":[],"env":{},"server_type":"stdio"})).unwrap()];
    settings.save_sync().unwrap();
    let configured = Settings::load_hierarchical(&project)
        .await
        .unwrap()
        .effective_config()
        .mcp_servers;
    let imported_roster = snapshot.mcp_roster(&configured);
    assert_eq!(imported_roster.len(), 4);
    assert_eq!(
        imported_roster
            .iter()
            .find(|s| s.name == "project-settings")
            .unwrap()
            .origin,
        McpServerOrigin::Project
    );
    let decision = claurst_core::mcp_trust::partition_mcp_servers(
        &imported_roster,
        Some(&project),
        false,
        &Default::default(),
        &Default::default(),
        &Default::default(),
    );
    assert_eq!(decision.allowed.len(), 2);
    assert_eq!(decision.pending.len(), 2);

    settings.disabled_plugins.insert("user".into());
    settings.save_sync().unwrap();
    assert!(matches!(
        ReloadPluginsCommand.execute("", &mut context).await,
        CommandResult::ReloadMcpRoster(_)
    ));
    assert!(!claurst_plugins::global_plugin_registry()
        .unwrap()
        .all_mcp_servers()
        .iter()
        .any(|s| s.name == "user-new"));
}
