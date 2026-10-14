/// Plugin registry — holds all loaded plugins and provides queries.
///
/// Ported from the TS "enabled plugins" concept in `pluginLoader.ts` and the
/// app-state plugin arrays.
use crate::plugin::{LoadedPlugin, PluginCommandDef, PluginError, PluginSource, ReloadDiff};
use claurst_core::config::{HookMap, HookPluginSource};
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// PluginRegistry
// ---------------------------------------------------------------------------

/// Central store for all discovered plugins in a session.
///
/// Methods follow the TS pattern: `enabled()` returns only enabled plugins,
/// `all()` returns every plugin including disabled ones.
#[derive(Debug, Default)]
pub struct PluginRegistry {
    /// All plugins keyed by name.
    plugins: HashMap<String, LoadedPlugin>,
    /// Names of plugins that are currently enabled.
    enabled_names: std::collections::HashSet<String>,
    /// Accumulated load errors.
    pub errors: Vec<PluginError>,
}

impl PluginRegistry {
    // ---- Construction & population ----------------------------------------

    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert (or replace) a loaded plugin.  Emits a duplicate error if
    /// a different path already holds a plugin with the same name.
    pub fn insert(&mut self, plugin: LoadedPlugin) {
        let name = plugin.name.clone();
        let enabled = plugin.enabled;

        if let Some(existing) = self.plugins.get(&name) {
            if existing.path != plugin.path {
                self.errors.push(PluginError::DuplicateName {
                    name: name.clone(),
                    first: existing.path.to_string_lossy().into_owned(),
                    second: plugin.path.to_string_lossy().into_owned(),
                });
                // Keep the first one (same behaviour as TS: first-wins).
                return;
            }
        }

        self.plugins.insert(name.clone(), plugin);
        if enabled {
            self.enabled_names.insert(name);
        }
    }

    /// Append multiple plugins at once, updating errors inline.
    pub fn extend(&mut self, plugins: Vec<LoadedPlugin>, errors: Vec<PluginError>) {
        self.errors.extend(errors);
        for p in plugins {
            self.insert(p);
        }
    }

    // ---- Queries ----------------------------------------------------------

    /// All loaded plugins (enabled + disabled).
    pub fn all(&self) -> Vec<&LoadedPlugin> {
        self.plugins.values().collect()
    }

    /// Only the enabled plugins.
    pub fn enabled(&self) -> Vec<&LoadedPlugin> {
        self.plugins
            .values()
            .filter(|p| self.enabled_names.contains(&p.name))
            .collect()
    }

    /// Look up a plugin by name.
    pub fn get(&self, name: &str) -> Option<&LoadedPlugin> {
        self.plugins.get(name)
    }

    /// Whether a plugin is enabled.
    pub fn is_enabled(&self, name: &str) -> bool {
        self.enabled_names.contains(name)
    }

    // ---- Enable / disable -------------------------------------------------

    /// Enable a plugin by name.  Returns `false` if the plugin is not loaded.
    pub fn enable(&mut self, name: &str) -> bool {
        if self.plugins.contains_key(name) {
            self.enabled_names.insert(name.to_string());
            if let Some(p) = self.plugins.get_mut(name) {
                p.enabled = true;
            }
            true
        } else {
            false
        }
    }

    /// Disable a plugin by name.  Returns `false` if the plugin is not loaded.
    pub fn disable(&mut self, name: &str) -> bool {
        if self.plugins.contains_key(name) {
            self.enabled_names.remove(name);
            if let Some(p) = self.plugins.get_mut(name) {
                p.enabled = false;
            }
            true
        } else {
            false
        }
    }

    // ---- Derived collections from enabled plugins -------------------------

    /// Collect all `PluginCommandDef` items from enabled plugins.
    pub fn all_command_defs(&self) -> Vec<PluginCommandDef> {
        let mut defs: Vec<PluginCommandDef> = Vec::new();
        for plugin in self.enabled() {
            let mut plugin_defs = crate::loader::collect_command_defs(plugin);
            // Patch source_id now that we have it.
            for d in &mut plugin_defs {
                d.plugin_source_id = plugin.source_id.clone();
            }
            defs.extend(plugin_defs);
        }
        defs
    }

    /// Collect paths to all `skills/` directories contributed by enabled plugins.
    pub fn all_skill_paths(&self) -> Vec<std::path::PathBuf> {
        let mut paths = Vec::new();
        for plugin in self.enabled() {
            if let Some(ref p) = plugin.skills_path {
                paths.push(p.clone());
            }
        }
        paths
    }

    /// Collect paths to all `agents/` directories contributed by enabled plugins.
    pub fn all_agent_paths(&self) -> Vec<std::path::PathBuf> {
        let mut paths = Vec::new();
        for plugin in self.enabled() {
            if let Some(ref p) = plugin.agents_path {
                paths.push(p.clone());
            }
        }
        paths
    }

    /// Collect paths to all `output-styles/` directories contributed by enabled plugins.
    pub fn all_output_style_paths(&self) -> Vec<std::path::PathBuf> {
        let mut paths = Vec::new();
        for plugin in self.enabled() {
            if let Some(ref p) = plugin.output_styles_path {
                paths.push(p.clone());
            }
        }
        paths
    }

    /// The hooks of every enabled plugin, for the core hook engine. Each
    /// hook carries its plugin's name and root. Project plugins are skipped
    /// (see `set_global_hooks`). Plugins are visited in name order so the run
    /// order of hooks across plugins is stable.
    pub fn hook_map(&self) -> HookMap {
        let mut plugins: Vec<&LoadedPlugin> = self
            .enabled()
            .into_iter()
            .filter(|plugin| plugin.source != PluginSource::Project)
            .collect();
        plugins.sort_by(|a, b| a.name.cmp(&b.name));
        let mut hooks = HookMap::new();
        for plugin in plugins {
            let Some(config) = &plugin.hooks_config else { continue };
            let source = HookPluginSource { name: plugin.name.clone(), root: plugin.path.clone() };
            for (event, groups) in &config.events {
                hooks.entry(*event).or_default().extend(groups.iter().map(|group| {
                    let mut group = group.clone();
                    for hook in &mut group.hooks {
                        hook.plugin = Some(source.clone());
                    }
                    group
                }));
            }
        }
        hooks
    }

    /// Collect all MCP server configs contributed by enabled plugins.
    ///
    /// Servers from project plugins (a repository's `.claurst/plugins/`) are
    /// tagged [`McpServerOrigin::Project`] so they go through the same
    /// approval gate as servers in a project's `.claurst/settings.json`: the
    /// repository, not the user, put them there (#493). Servers from user and
    /// `--plugin-dir` plugins stay [`McpServerOrigin::User`].
    ///
    /// [`McpServerOrigin::Project`]: claurst_core::config::McpServerOrigin::Project
    /// [`McpServerOrigin::User`]: claurst_core::config::McpServerOrigin::User
    pub fn all_mcp_servers(&self) -> Vec<claurst_core::config::McpServerConfig> {
        use claurst_core::config::McpServerOrigin;
        let mut servers: Vec<claurst_core::config::McpServerConfig> = Vec::new();
        for plugin in self.enabled() {
            let origin = if plugin.source == PluginSource::Project {
                McpServerOrigin::Project
            } else {
                McpServerOrigin::User
            };
            for mcp in &plugin.manifest.mcp_servers {
                servers.push(claurst_core::config::McpServerConfig {
                    name: mcp.name.clone(),
                    command: mcp.command.clone(),
                    args: mcp.args.clone(),
                    env: mcp.env.clone(),
                    url: mcp.url.clone(),
                    server_type: mcp.server_type.clone(),
                    headers: Default::default(),
                    connect_timeout_ms: None,
                    catalog_timeout_ms: None,
                    origin,
                });
            }
        }
        servers
    }

    /// Collect all LSP server configs contributed by enabled plugins.
    pub fn all_lsp_servers(&self) -> Vec<crate::manifest::PluginLspServer> {
        let mut servers: Vec<crate::manifest::PluginLspServer> = Vec::new();
        for plugin in self.enabled() {
            for lsp in &plugin.manifest.lsp_servers {
                servers.push(lsp.clone());
            }
        }
        servers
    }

    // ---- Statistics -------------------------------------------------------

    /// Total number of plugins (enabled + disabled).
    pub fn plugin_count(&self) -> usize {
        self.plugins.len()
    }

    /// Number of enabled plugins.
    pub fn enabled_count(&self) -> usize {
        self.enabled_names.len()
    }

    /// Number of load errors.
    pub fn error_count(&self) -> usize {
        self.errors.len()
    }

    // ---- Reload diff ------------------------------------------------------

    /// Compare this registry against `old` and produce a diff report.
    pub fn diff_against(&self, old: &PluginRegistry) -> ReloadDiff {
        let old_names: std::collections::HashSet<&str> =
            old.plugins.keys().map(|s| s.as_str()).collect();
        let new_names: std::collections::HashSet<&str> =
            self.plugins.keys().map(|s| s.as_str()).collect();

        let added: Vec<String> = new_names
            .difference(&old_names)
            .map(|&s| s.to_string())
            .collect();
        let removed: Vec<String> = old_names
            .difference(&new_names)
            .map(|&s| s.to_string())
            .collect();
        let updated: Vec<String> = new_names
            .intersection(&old_names)
            .filter(|&&name| {
                let new_ver = self
                    .plugins
                    .get(name)
                    .and_then(|p| p.manifest.version.as_deref());
                let old_ver = old
                    .plugins
                    .get(name)
                    .and_then(|p| p.manifest.version.as_deref());
                new_ver != old_ver
            })
            .map(|&s| s.to_string())
            .collect();

        ReloadDiff {
            added,
            removed,
            updated,
            error_count: self.errors.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::PluginSource;
    use crate::manifest::PluginManifest;
    use std::path::PathBuf;

    fn make_plugin(name: &str) -> LoadedPlugin {
        LoadedPlugin {
            name: name.to_string(),
            path: PathBuf::from(format!("/tmp/{}", name)),
            source: PluginSource::User,
            source_id: format!("{}@user", name),
            manifest: PluginManifest {
                name: name.to_string(),
                ..Default::default()
            },
            enabled: true,
            commands_path: None,
            agents_path: None,
            skills_path: None,
            output_styles_path: None,
            hooks_config: None,
        }
    }

    #[test]
    fn enable_disable() {
        let mut reg = PluginRegistry::new();
        reg.insert(make_plugin("alpha"));
        assert!(reg.is_enabled("alpha"));

        reg.disable("alpha");
        assert!(!reg.is_enabled("alpha"));
        assert_eq!(reg.enabled().len(), 0);

        reg.enable("alpha");
        assert!(reg.is_enabled("alpha"));
        assert_eq!(reg.enabled().len(), 1);
    }

    #[test]
    fn duplicate_name_kept_first() {
        let mut reg = PluginRegistry::new();
        reg.insert(make_plugin("beta"));
        let mut dup = make_plugin("beta");
        dup.path = PathBuf::from("/tmp/beta2");
        reg.insert(dup);
        assert_eq!(reg.plugin_count(), 1);
        assert_eq!(reg.error_count(), 1);
    }

    #[test]
    fn diff_detects_added_removed() {
        let mut old_reg = PluginRegistry::new();
        old_reg.insert(make_plugin("kept"));
        old_reg.insert(make_plugin("gone"));

        let mut new_reg = PluginRegistry::new();
        new_reg.insert(make_plugin("kept"));
        new_reg.insert(make_plugin("new-plugin"));

        let diff = new_reg.diff_against(&old_reg);
        assert_eq!(diff.added, vec!["new-plugin"]);
        assert_eq!(diff.removed, vec!["gone"]);
        assert!(diff.updated.is_empty());
    }

    fn with_mcp_server(mut plugin: LoadedPlugin, server: &str) -> LoadedPlugin {
        plugin.manifest.mcp_servers.push(crate::manifest::PluginMcpServer {
            name: server.to_string(),
            command: Some("server-bin".to_string()),
            args: vec![],
            env: Default::default(),
            url: None,
            server_type: "stdio".to_string(),
        });
        plugin
    }

    #[test]
    fn mcp_server_origin_follows_plugin_source() {
        use claurst_core::config::McpServerOrigin;
        let mut reg = PluginRegistry::new();
        reg.insert(with_mcp_server(make_plugin("user-plugin"), "user-srv"));
        let mut project = with_mcp_server(make_plugin("repo-plugin"), "repo-srv");
        project.source = PluginSource::Project;
        reg.insert(project);
        let mut extra = with_mcp_server(make_plugin("extra-plugin"), "extra-srv");
        extra.source = PluginSource::Extra("/opt/plugins".to_string());
        reg.insert(extra);

        let origin_of = |name: &str| {
            reg.all_mcp_servers()
                .into_iter()
                .find(|s| s.name == name)
                .map(|s| s.origin)
        };
        // A repository's own plugin must not auto-launch its MCP server (#493).
        assert_eq!(origin_of("repo-srv"), Some(McpServerOrigin::Project));
        assert_eq!(origin_of("user-srv"), Some(McpServerOrigin::User));
        assert_eq!(origin_of("extra-srv"), Some(McpServerOrigin::User));
    }

    #[test]
    fn project_plugin_mcp_server_is_held_by_trust_gate() {
        use claurst_core::mcp_trust::{partition_mcp_servers, McpTrustStore};
        let mut reg = PluginRegistry::new();
        reg.insert(with_mcp_server(make_plugin("user-plugin"), "user-srv"));
        let mut project = with_mcp_server(make_plugin("repo-plugin"), "repo-srv");
        project.source = PluginSource::Project;
        reg.insert(project);

        let decision = partition_mcp_servers(
            &reg.all_mcp_servers(),
            None,
            false,
            &std::collections::HashSet::new(),
            &McpTrustStore::default(),
        );
        let names = |v: &[claurst_core::config::McpServerConfig]| {
            v.iter().map(|s| s.name.clone()).collect::<Vec<_>>()
        };
        assert_eq!(names(&decision.allowed), vec!["user-srv"]);
        assert_eq!(names(&decision.pending), vec!["repo-srv"]);
    }
}
