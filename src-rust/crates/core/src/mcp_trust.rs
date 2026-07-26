//! Trust gating for project-defined MCP servers.
//!
//! A repository can ship a `.claurst/settings.json` that declares MCP servers
//! with an arbitrary `command`. With the stdio transport, launching such a
//! server spawns a child process — so auto-launching project-defined servers
//! on open is remote code execution: cloning and opening a malicious repo would
//! run attacker-controlled code with no consent.
//!
//! This module implements the trust model that gates those servers:
//!
//! - Servers with [`McpServerOrigin::User`] (global settings, `--mcp-config`,
//!   enabled plugins) are always allowed — no behavior change.
//! - Servers with [`McpServerOrigin::Project`] are only launched if the user
//!   has approved them, either:
//!     * globally (`trustProjectMcpServers` setting / `--trust-project-mcp`),
//!     * for this session (in-memory `session_trusted` set), or
//!     * persistently, recorded in a per-user allowlist keyed by project root
//!       and a fingerprint of the server's launch identity.
//!
//! The persistent allowlist lives under the user's config dir
//! (`~/.claurst/mcp_trust.json`), NOT in the repository, so a repo can never
//! grant itself trust.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{McpServerConfig, McpServerOrigin, Settings};

/// Path to the per-user project-MCP trust store.
///
/// Stored alongside the global settings (`~/.claurst/mcp_trust.json`) and never
/// inside a repository.
pub fn trust_store_path() -> PathBuf {
    Settings::config_dir().join("mcp_trust.json")
}

/// Compute a stable fingerprint of a server's launch identity.
///
/// The fingerprint covers the fields that determine *what actually runs*
/// (name, transport, command, args, url). If a repo later changes the command
/// behind an approved server name, the fingerprint changes and the user is
/// re-prompted — approval cannot be silently re-pointed at a new binary.
///
/// `env` is intentionally excluded: env values are frequently host-specific
/// (expanded at launch) and not themselves the executable, so including them
/// would cause spurious re-prompts without meaningfully tightening the gate.
pub fn server_fingerprint(cfg: &McpServerConfig) -> String {
    use sha2::{Digest, Sha256};
    // Length-prefix each field so distinct field boundaries can't collide
    // (e.g. command="ab" args=["c"] vs command="a" args=["bc"]).
    fn feed(hasher: &mut Sha256, s: &str) {
        hasher.update((s.len() as u64).to_le_bytes());
        hasher.update(s.as_bytes());
    }
    let mut hasher = Sha256::new();
    feed(&mut hasher, &cfg.name);
    feed(&mut hasher, &cfg.server_type);
    feed(&mut hasher, cfg.command.as_deref().unwrap_or(""));
    feed(&mut hasher, cfg.url.as_deref().unwrap_or(""));
    hasher.update((cfg.args.len() as u64).to_le_bytes());
    for a in &cfg.args {
        feed(&mut hasher, a);
    }
    // Environment variables are part of WHAT executes: LD_PRELOAD, LD_LIBRARY_PATH,
    // DYLD_INSERT_LIBRARIES, PYTHONPATH/RUBYLIB/PERL5LIB, PATH, etc. can redirect a
    // benign-looking command to attacker code. So a change to env must re-prompt;
    // excluding it would let an already-approved project inject code via env alone.
    // Sort first — HashMap iteration order is non-deterministic.
    let mut env: Vec<(&String, &String)> = cfg.env.iter().collect();
    env.sort();
    hasher.update((env.len() as u64).to_le_bytes());
    for (k, v) in env {
        feed(&mut hasher, k);
        feed(&mut hasher, v);
    }
    hex::encode(hasher.finalize())
}

/// Canonicalize a project root for use as a stable allowlist key.
fn project_key(project_root: &Path) -> String {
    std::fs::canonicalize(project_root)
        .unwrap_or_else(|_| project_root.to_path_buf())
        .to_string_lossy()
        .to_string()
}

/// Find the project root for `cwd`: the nearest ancestor directory that
/// contains a `.claurst/settings.json(c)` and is *not* the global config dir.
///
/// Returns `None` when there is no project-level settings file above `cwd`.
/// This mirrors the walk in `Settings::find_project_settings` so trust
/// approvals are keyed by the same directory the project config came from.
pub fn project_root_for(cwd: &Path) -> Option<PathBuf> {
    let global = Settings::config_dir();
    let mut dir = cwd;
    loop {
        let claurst = dir.join(".claurst");
        if claurst != global {
            for name in ["settings.json", "settings.jsonc"] {
                if claurst.join(name).exists() {
                    return Some(dir.to_path_buf());
                }
            }
        }
        dir = dir.parent()?;
    }
}

/// Per-user allowlist of approved project MCP servers.
///
/// `approvals` maps a canonical project-root path to the set of approved
/// server fingerprints for that project.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpTrustStore {
    #[serde(default)]
    pub approvals: HashMap<String, HashSet<String>>,
}

impl McpTrustStore {
    /// Load the store from disk, returning an empty store if the file is
    /// missing or unreadable/corrupt (best-effort; never fails the session).
    pub fn load() -> Self {
        let path = trust_store_path();
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Persist the store to disk (`~/.claurst/mcp_trust.json`).
    pub fn save(&self) -> std::io::Result<()> {
        let path = trust_store_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)
            .map_err(std::io::Error::other)?;
        // Write-then-rename so a concurrent reader never sees a half-written
        // (corrupt) trust file. Rename is atomic within the same directory.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, &path)
    }

    /// Whether `cfg` has been persistently approved for `project_root`.
    pub fn is_approved(&self, project_root: &Path, cfg: &McpServerConfig) -> bool {
        let key = project_key(project_root);
        self.approvals
            .get(&key)
            .is_some_and(|set| set.contains(&server_fingerprint(cfg)))
    }

    /// Record a persistent approval for `cfg` under `project_root`.
    pub fn approve(&mut self, project_root: &Path, cfg: &McpServerConfig) {
        let key = project_key(project_root);
        self.approvals
            .entry(key)
            .or_default()
            .insert(server_fingerprint(cfg));
    }
}

/// The result of gating a roster of MCP servers.
#[derive(Debug, Clone, Default)]
pub struct McpGateDecision {
    /// Servers cleared to auto-launch (all user-origin servers, plus any
    /// approved project servers).
    pub allowed: Vec<McpServerConfig>,
    /// Project-origin servers that are NOT yet approved. These must not be
    /// launched without explicit user consent (TUI prompt) or an opt-in flag.
    pub pending: Vec<McpServerConfig>,
}

/// Partition `servers` into those allowed to auto-launch and those still
/// pending approval.
///
/// - [`McpServerOrigin::User`] servers are always allowed.
/// - [`McpServerOrigin::Project`] servers are allowed only when any of:
///     * `trust_all_project` is set (global opt-in / CLI flag),
///     * the fingerprint is in `session_trusted` (approved this session), or
///     * the fingerprint is persistently approved for `project_root`.
///
/// When `project_root` is `None`, project servers can only be cleared via
/// `trust_all_project` or `session_trusted` (there is nowhere to look up a
/// persisted approval).
pub fn partition_mcp_servers(
    servers: &[McpServerConfig],
    project_root: Option<&Path>,
    trust_all_project: bool,
    session_trusted: &HashSet<String>,
    store: &McpTrustStore,
) -> McpGateDecision {
    let mut decision = McpGateDecision::default();
    for server in servers {
        match server.origin {
            McpServerOrigin::User => decision.allowed.push(server.clone()),
            McpServerOrigin::Project => {
                let approved = trust_all_project
                    || session_trusted.contains(&server_fingerprint(server))
                    || project_root.is_some_and(|root| store.is_approved(root, server));
                if approved {
                    decision.allowed.push(server.clone());
                } else {
                    decision.pending.push(server.clone());
                }
            }
        }
    }
    decision
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_server(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            command: Some("user-bin".to_string()),
            args: vec![],
            env: HashMap::new(),
            url: None,
            server_type: "stdio".to_string(),
            origin: McpServerOrigin::User,
        }
    }

    fn project_server(name: &str, command: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            command: Some(command.to_string()),
            args: vec!["--flag".to_string()],
            env: HashMap::new(),
            url: None,
            server_type: "stdio".to_string(),
            origin: McpServerOrigin::Project,
        }
    }

    #[test]
    fn user_servers_always_allowed() {
        let servers = vec![user_server("a"), user_server("b")];
        let store = McpTrustStore::default();
        let d = partition_mcp_servers(&servers, None, false, &HashSet::new(), &store);
        assert_eq!(d.allowed.len(), 2);
        assert!(d.pending.is_empty());
    }

    #[test]
    fn project_servers_pending_by_default() {
        let servers = vec![user_server("u"), project_server("p", "evil")];
        let store = McpTrustStore::default();
        let d = partition_mcp_servers(&servers, None, false, &HashSet::new(), &store);
        assert_eq!(d.allowed.len(), 1);
        assert_eq!(d.allowed[0].name, "u");
        assert_eq!(d.pending.len(), 1);
        assert_eq!(d.pending[0].name, "p");
    }

    #[test]
    fn trust_all_clears_project_servers() {
        let servers = vec![project_server("p", "evil")];
        let store = McpTrustStore::default();
        let d = partition_mcp_servers(&servers, None, true, &HashSet::new(), &store);
        assert_eq!(d.allowed.len(), 1);
        assert!(d.pending.is_empty());
    }

    #[test]
    fn session_trust_clears_only_matching_fingerprint() {
        let p = project_server("p", "evil");
        let mut session = HashSet::new();
        session.insert(server_fingerprint(&p));
        let store = McpTrustStore::default();
        // Same identity: allowed.
        let d = partition_mcp_servers(std::slice::from_ref(&p), None, false, &session, &store);
        assert_eq!(d.allowed.len(), 1);
        assert!(d.pending.is_empty());
        // Different command under the same name: re-prompted (not allowed).
        let p2 = project_server("p", "different-binary");
        let d2 = partition_mcp_servers(&[p2], None, false, &session, &store);
        assert!(d2.allowed.is_empty());
        assert_eq!(d2.pending.len(), 1);
    }

    #[test]
    fn fingerprint_is_stable_and_command_sensitive() {
        let a = project_server("srv", "cmd-one");
        let b = project_server("srv", "cmd-one");
        let c = project_server("srv", "cmd-two");
        assert_eq!(server_fingerprint(&a), server_fingerprint(&b));
        assert_ne!(server_fingerprint(&a), server_fingerprint(&c));
    }

    #[test]
    fn fingerprint_is_env_sensitive_and_order_independent() {
        // Adding an env var (e.g. LD_PRELOAD) to an already-approved server must
        // change the fingerprint so it re-prompts — env is part of what executes.
        let base = project_server("srv", "cmd");
        let mut injected = project_server("srv", "cmd");
        injected
            .env
            .insert("LD_PRELOAD".to_string(), "/tmp/evil.so".to_string());
        assert_ne!(
            server_fingerprint(&base),
            server_fingerprint(&injected),
            "env injection must change the fingerprint"
        );

        // The same env, inserted in a different order, must hash identically
        // (HashMap iteration order must not produce spurious re-prompts).
        let mut x = project_server("srv", "cmd");
        x.env.insert("A".to_string(), "1".to_string());
        x.env.insert("B".to_string(), "2".to_string());
        let mut y = project_server("srv", "cmd");
        y.env.insert("B".to_string(), "2".to_string());
        y.env.insert("A".to_string(), "1".to_string());
        assert_eq!(server_fingerprint(&x), server_fingerprint(&y));
    }

    #[test]
    fn persisted_approval_roundtrips() {
        let root = std::env::temp_dir().join(format!(
            "claurst-mcp-trust-test-{}",
            uuid::Uuid::new_v4()
        ));
        let server = project_server("p", "evil");
        let mut store = McpTrustStore::default();
        assert!(!store.is_approved(&root, &server));
        store.approve(&root, &server);
        assert!(store.is_approved(&root, &server));

        // A persisted approval clears the gate even with no session trust.
        let d = partition_mcp_servers(
            std::slice::from_ref(&server),
            Some(root.as_path()),
            false,
            &HashSet::new(),
            &store,
        );
        assert_eq!(d.allowed.len(), 1);
        assert!(d.pending.is_empty());

        // Serialization round-trips the approval set.
        let json = serde_json::to_string(&store).unwrap();
        let restored: McpTrustStore = serde_json::from_str(&json).unwrap();
        assert!(restored.is_approved(&root, &server));
    }
}
