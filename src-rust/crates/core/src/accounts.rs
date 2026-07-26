//! Multi-account credential management.
//!
//! Stores named profiles per provider (anthropic, codex) on disk so users can
//! switch between Pro/Max/work/personal accounts without re-logging-in.
//!
//! Design borrows from two prior arts:
//!
//!   * **codexmaxx** (kitze/codexmaxx) — named per-account snapshots stored on
//!     disk, identity derived from JWT payload (email / account_id), explicit
//!     "import current external login" flow.
//!   * **opencode** — single tagged-union JSON file, chmod 0600, symmetric
//!     `list / login / logout / switch` commands across providers.
//!
//! Layout:
//!
//! ```text
//! ~/.claurst/
//!   accounts.json                              # registry (this module)
//!   accounts/
//!     anthropic/<profile-id>/oauth_tokens.json
//!     codex/<profile-id>/codex_tokens.json
//!   oauth_tokens.json                          # legacy (auto-migrated)
//!   codex_tokens.json                          # legacy (auto-migrated)
//! ```
//!
//! The registry holds metadata (label, email, account-id, timestamps, active
//! pointer per provider). The per-account credential files keep their existing
//! schemas so the rest of the codebase doesn't change shape.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Identifier for a credential provider that supports multi-account.
pub const PROVIDER_ANTHROPIC: &str = "anthropic";
pub const PROVIDER_CODEX: &str = "codex";

/// Metadata recorded for a single stored profile.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountProfile {
    /// Slug used as the directory name and CLI identifier.
    pub id: String,
    /// Optional human-friendly label (e.g. "work", "personal").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Email extracted from the JWT id_token (when available).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Provider-side account identifier (account_id / account_uuid).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Organization UUID (Anthropic only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization_uuid: Option<String>,
    /// Plan / subscription tier (Pro, Max, …) when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_tier: Option<String>,
    /// ISO-8601 timestamp when this profile was first added.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_at: Option<String>,
    /// ISO-8601 timestamp of the last `switch_to(...)` call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_selected_at: Option<String>,
}

impl AccountProfile {
    /// Best-effort display name for menus: label > email > id.
    pub fn display_name(&self) -> String {
        self.label
            .clone()
            .or_else(|| self.email.clone())
            .unwrap_or_else(|| self.id.clone())
    }
}

/// Per-provider section of the registry.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderAccounts {
    /// Profile id of the currently-active account for this provider.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    /// All stored profiles, keyed by id.
    #[serde(default)]
    pub profiles: BTreeMap<String, AccountProfile>,
}

/// On-disk shape of `~/.claurst/accounts.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccountRegistry {
    /// Schema version (current: 1).
    #[serde(default = "default_version")]
    pub version: u32,
    /// One entry per credential provider.
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderAccounts>,
}

fn default_version() -> u32 {
    1
}

impl AccountRegistry {
    /// Path to `~/.claurst/accounts.json`.
    pub fn path() -> PathBuf {
        claurst_dir().join("accounts.json")
    }

    /// Load the registry. Returns an empty registry if the file is missing or
    /// malformed.
    pub fn load() -> Self {
        let path = Self::path();
        if let Ok(data) = std::fs::read_to_string(&path) {
            if let Ok(reg) = serde_json::from_str::<AccountRegistry>(&data) {
                return reg;
            }
        }
        AccountRegistry::default()
    }

    /// Persist the registry to disk. Best-effort but propagates I/O errors.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            set_user_only_dir_perms(parent);
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;
        set_user_only_perms(&path);
        Ok(())
    }

    /// Get the active profile id for a provider.
    pub fn active(&self, provider: &str) -> Option<&str> {
        self.providers
            .get(provider)
            .and_then(|p| p.active.as_deref())
    }

    /// Get the active profile metadata, if any.
    pub fn active_profile(&self, provider: &str) -> Option<&AccountProfile> {
        let p = self.providers.get(provider)?;
        let id = p.active.as_ref()?;
        p.profiles.get(id)
    }

    /// List all profiles for a provider (sorted by id).
    pub fn list(&self, provider: &str) -> Vec<AccountProfile> {
        self.providers
            .get(provider)
            .map(|p| p.profiles.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Lookup a profile by id within a provider.
    pub fn get(&self, provider: &str, id: &str) -> Option<&AccountProfile> {
        self.providers.get(provider)?.profiles.get(id)
    }

    /// Insert or update a profile, optionally setting it active.
    pub fn upsert(
        &mut self,
        provider: &str,
        mut profile: AccountProfile,
        make_active: bool,
    ) -> anyhow::Result<()> {
        if profile.added_at.is_none() {
            profile.added_at = Some(now_iso());
        }
        let section = self.providers.entry(provider.to_string()).or_default();
        section.profiles.insert(profile.id.clone(), profile.clone());
        if make_active {
            section.active = Some(profile.id.clone());
            if let Some(stored) = section.profiles.get_mut(&profile.id) {
                stored.last_selected_at = Some(now_iso());
            }
        }
        self.save()
    }

    /// Switch the active profile for a provider. Returns `Err` if the id does
    /// not exist.
    pub fn switch_to(&mut self, provider: &str, id: &str) -> anyhow::Result<()> {
        let section = self
            .providers
            .get_mut(provider)
            .ok_or_else(|| anyhow::anyhow!("No accounts stored for {provider}"))?;
        if !section.profiles.contains_key(id) {
            anyhow::bail!("Account '{}' not found for {}", id, provider);
        }
        section.active = Some(id.to_string());
        if let Some(p) = section.profiles.get_mut(id) {
            p.last_selected_at = Some(now_iso());
        }
        self.save()
    }

    /// Remove a profile (and its credential directory). If it was active,
    /// clears the active pointer.
    pub fn remove(&mut self, provider: &str, id: &str) -> anyhow::Result<()> {
        if let Some(section) = self.providers.get_mut(provider) {
            section.profiles.remove(id);
            if section.active.as_deref() == Some(id) {
                section.active = None;
            }
        }
        // Remove the per-account credential dir.
        let dir = account_dir(provider, id);
        if dir.exists() {
            let _ = std::fs::remove_dir_all(&dir);
        }
        self.save()
    }
}

/// Slugify an arbitrary string into a safe profile id. Lowercases, replaces
/// non-`[a-z0-9_-]` with `-`, trims dashes/underscores from edges, falls back
/// to "account" if the result is empty.
pub fn slugify_profile_id(raw: &str) -> String {
    let lowered = raw.trim().to_lowercase();
    let mapped: String = lowered
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    let trimmed = mapped.trim_matches(|c: char| c == '-' || c == '_').to_string();
    if trimmed.is_empty() {
        "account".to_string()
    } else {
        trimmed
    }
}

/// If the requested id already exists, suffix with -2, -3, … until free.
pub fn ensure_unique_profile_id(
    registry: &AccountRegistry,
    provider: &str,
    base: &str,
) -> String {
    let base = slugify_profile_id(base);
    if registry.get(provider, &base).is_none() {
        return base;
    }
    let mut n = 2usize;
    loop {
        let candidate = format!("{}-{}", base, n);
        if registry.get(provider, &candidate).is_none() {
            return candidate;
        }
        n += 1;
    }
}

/// The canonical claurst home directory.
pub fn claurst_dir() -> PathBuf {
    crate::config::Settings::config_dir()
}

/// `~/.claurst/accounts/<provider>/<id>/`.
pub fn account_dir(provider: &str, id: &str) -> PathBuf {
    claurst_dir().join("accounts").join(provider).join(id)
}

/// File where the per-account Anthropic OAuth tokens live.
pub fn anthropic_token_path(profile_id: &str) -> PathBuf {
    account_dir(PROVIDER_ANTHROPIC, profile_id).join("oauth_tokens.json")
}

/// File where the per-account Codex OAuth tokens live.
pub fn codex_token_path(profile_id: &str) -> PathBuf {
    account_dir(PROVIDER_CODEX, profile_id).join("codex_tokens.json")
}

/// Backup directory for the previous live token file (rotated on each switch).
pub fn backup_dir(provider: &str) -> PathBuf {
    claurst_dir().join("accounts").join(provider).join(".backups")
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Tighten permissions on a credential/session file so only the owner can
/// read or write it (mode `0o600`). Best-effort and Unix-only; a no-op on
/// other platforms (Windows ACLs are out of scope). Shared across modules
/// that persist tokens, credentials, or session transcripts (issue #212).
#[allow(unused_variables)]
pub(crate) fn set_user_only_perms(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(path, perms);
        }
    }
}

/// Tighten permissions on a directory that holds credentials/session data so
/// only the owner can traverse or list it (mode `0o700`). Best-effort and
/// Unix-only; a no-op on other platforms (issue #212).
#[allow(unused_variables)]
pub(crate) fn set_user_only_dir_perms(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o700);
            let _ = std::fs::set_permissions(path, perms);
        }
    }
}

// ---------------------------------------------------------------------------
// JWT identity extraction
// ---------------------------------------------------------------------------

/// Identity fields extracted from an OpenAI/Codex id_token or access_token.
#[derive(Debug, Clone, Default)]
pub struct JwtIdentity {
    pub email: Option<String>,
    pub account_id: Option<String>,
}

/// Decode the payload of a JWT (`header.payload.signature`) and pull out the
/// fields we care about for naming a profile. Tolerates malformed input by
/// returning an empty identity.
pub fn jwt_identity(token: &str) -> JwtIdentity {
    use base64::Engine;

    let mut out = JwtIdentity::default();
    let parts: Vec<&str> = token.splitn(3, '.').collect();
    let Some(payload_b64) = parts.get(1) else {
        return out;
    };

    // JWT payloads are base64url-encoded without padding.
    let mut padded = (*payload_b64).to_string();
    while padded.len() % 4 != 0 {
        padded.push('=');
    }
    let bytes = match base64::engine::general_purpose::URL_SAFE.decode(padded.as_bytes()) {
        Ok(b) => b,
        Err(_) => return out,
    };
    let json: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return out,
    };

    // Direct email claim wins; otherwise look at the OpenAI custom profile claim.
    if let Some(email) = json.get("email").and_then(|v| v.as_str()) {
        out.email = Some(email.to_string());
    } else if let Some(profile) = json
        .get("https://api.openai.com/profile")
        .and_then(|v| v.as_object())
    {
        if let Some(email) = profile.get("email").and_then(|v| v.as_str()) {
            out.email = Some(email.to_string());
        }
    }

    // OpenAI puts account_id under the custom auth claim.
    if let Some(auth) = json
        .get("https://api.openai.com/auth")
        .and_then(|v| v.as_object())
    {
        if let Some(id) = auth.get("account_id").and_then(|v| v.as_str()) {
            out.account_id = Some(id.to_string());
        }
    }

    out
}

/// Derive a short, human-friendly profile id from a JWT identity. Falls back
/// to "account" if nothing useful is in the token.
pub fn id_from_identity(identity: &JwtIdentity) -> String {
    if let Some(email) = &identity.email {
        // Use the local-part of the email (before @) as the slug source.
        let local = email.split('@').next().unwrap_or(email);
        return slugify_profile_id(local);
    }
    if let Some(account_id) = &identity.account_id {
        return slugify_profile_id(account_id);
    }
    "account".to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_strips_punctuation_and_lowercases() {
        assert_eq!(slugify_profile_id("Work Account!"), "work-account");
        assert_eq!(slugify_profile_id("  --weird-- "), "weird");
        assert_eq!(slugify_profile_id(""), "account");
        assert_eq!(slugify_profile_id("kuber@example.com"), "kuber-example-com");
    }

    #[test]
    fn ensure_unique_appends_suffix() {
        let mut reg = AccountRegistry::default();
        let mut section = ProviderAccounts::default();
        section.profiles.insert(
            "work".to_string(),
            AccountProfile { id: "work".into(), ..Default::default() },
        );
        reg.providers.insert(PROVIDER_ANTHROPIC.into(), section);

        let next = ensure_unique_profile_id(&reg, PROVIDER_ANTHROPIC, "work");
        assert_eq!(next, "work-2");
        let fresh = ensure_unique_profile_id(&reg, PROVIDER_ANTHROPIC, "personal");
        assert_eq!(fresh, "personal");
    }

    #[test]
    fn jwt_identity_is_lenient_to_garbage() {
        let identity = jwt_identity("not.a.jwt");
        assert!(identity.email.is_none());
        assert!(identity.account_id.is_none());

        let empty = jwt_identity("");
        assert!(empty.email.is_none());
    }

    #[test]
    fn jwt_identity_pulls_email_and_account_id() {
        use base64::Engine;
        let payload = serde_json::json!({
            "email": "kuber@example.com",
            "https://api.openai.com/auth": {
                "account_id": "acc_abc123"
            }
        });
        let payload_b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_string(&payload).unwrap());
        let token = format!("header.{}.signature", payload_b64);
        let identity = jwt_identity(&token);
        assert_eq!(identity.email.as_deref(), Some("kuber@example.com"));
        assert_eq!(identity.account_id.as_deref(), Some("acc_abc123"));

        assert_eq!(id_from_identity(&identity), "kuber");
    }

    #[test]
    fn account_paths_are_under_claurst_dir() {
        let p = anthropic_token_path("work");
        assert!(p.ends_with("accounts/anthropic/work/oauth_tokens.json"));
        let c = codex_token_path("personal");
        assert!(c.ends_with("accounts/codex/personal/codex_tokens.json"));
    }

    #[test]
    fn account_profile_display_falls_back_through_label_email_id() {
        let mut p = AccountProfile { id: "kuber".into(), ..Default::default() };
        assert_eq!(p.display_name(), "kuber");
        p.email = Some("kuber@example.com".into());
        assert_eq!(p.display_name(), "kuber@example.com");
        p.label = Some("Personal".into());
        assert_eq!(p.display_name(), "Personal");
    }

    // -----------------------------------------------------------------------
    // Issue #212: credential/session files must be owner-only (0o600) and
    // their parent dirs owner-only (0o700). Unix-only; a no-op elsewhere.
    // -----------------------------------------------------------------------

    /// The shared helper actually tightens a permissive file down to 0o600.
    #[cfg(unix)]
    #[test]
    fn set_user_only_perms_forces_file_to_0600() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("oauth_tokens.json");
        std::fs::write(&path, "{\"access_token\":\"secret\"}").unwrap();
        // Start deliberately world/group readable to prove we lock it down.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        set_user_only_perms(&path);

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "token file must be owner-only");
    }

    /// The dir helper tightens a permissive directory down to 0o700.
    #[cfg(unix)]
    #[test]
    fn set_user_only_dir_perms_forces_dir_to_0700() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("accounts");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        set_user_only_dir_perms(&dir);

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "account dir must be owner-only");
    }

    /// End-to-end: the real codex token save path produces a 0o600 file under
    /// a 0o700 account dir. Redirects `dirs::home_dir()` via a temp `HOME`,
    /// serialized against any other HOME-mutating test in this binary.
    #[cfg(unix)]
    #[test]
    fn real_codex_save_path_writes_0600_token_file() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::Mutex;
        static HOME_LOCK: Mutex<()> = Mutex::new(());
        let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let tmp = tempfile::tempdir().unwrap();
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());

        let tokens = crate::oauth_config::CodexTokens {
            access_token: "access-secret".into(),
            refresh_token: Some("refresh-secret".into()),
            account_id: Some("acc_1".into()),
            expires_at: Some(0),
        };
        let save_res = crate::oauth_config::save_codex_tokens_for_profile(&tokens, "work");

        let path = codex_token_path("work");
        let file_mode = std::fs::metadata(&path).map(|m| m.permissions().mode() & 0o777);
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .map(|m| m.permissions().mode() & 0o777);

        // Restore HOME before asserting so a failure can't leak the override
        // into the rest of the test binary.
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }

        save_res.unwrap();
        assert_eq!(file_mode.unwrap(), 0o600, "codex token file must be owner-only");
        assert_eq!(dir_mode.unwrap(), 0o700, "codex account dir must be owner-only");
    }
}
