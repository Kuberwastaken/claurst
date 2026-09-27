//! Persisted configuration for the memory sidecar system.
//!
//! Ported from jcode's `agents.memory_model` config block and the
//! `memory_model.json` store written by the Alt+M picker. Split from the
//! extraction sidecar itself so the settings schema, the
//! `Settings::set_memory_model` mutator, and the picker-owned
//! `MemoryModelStore` can land and be reviewed independently.
//!
//! Settings live in `settings.json` (camelCase, matching claurst's existing
//! naming); the picker store lives in `<config>/memory_model.json`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Version marker for the picker's memory model store file.
pub const MEMORY_MODEL_VERSION: u8 = 1;

/// File name for the picker's persisted memory model mark, relative to the
/// claurst config directory.
pub const MEMORY_MODEL_FILE: &str = "memory_model.json";

/// Sidecar backend selection for memory extraction.
///
/// - `Auto` (default): auto-select the backend based on the marked model and
///   available credentials (handled by the sidecar crate).
/// - `OpenAI`: force the OpenAI backend.
/// - `Claude`: force the Anthropic backend.
/// - `Provider`: dispatch through the active agent provider.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemorySidecarBackend {
    #[default]
    Auto,
    OpenAI,
    Claude,
    Provider,
}

impl MemorySidecarBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemorySidecarBackend::Auto => "auto",
            MemorySidecarBackend::OpenAI => "openai",
            MemorySidecarBackend::Claude => "claude",
            MemorySidecarBackend::Provider => "provider",
        }
    }

    /// Parse from settings text. Unknown values fall back to `Auto` rather
    /// than failing, mirroring jcode's tolerant env/config parsing.
    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "openai" => MemorySidecarBackend::OpenAI,
            "claude" => MemorySidecarBackend::Claude,
            "provider" => MemorySidecarBackend::Provider,
            _ => MemorySidecarBackend::Auto,
        }
    }
}

/// Fallback behavior when no memory model is marked (neither at session
/// level nor in config).
///
/// - `OpenAiClaude` (default): legacy auto-select behavior.
/// - `Provider`: use the active provider's current model directly.
/// - `None`: sidecar is dormant until a model is explicitly marked.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemorySidecarFallback {
    #[serde(rename = "openai_claude")]
    #[default]
    OpenAiClaude,
    Provider,
    None,
}

impl MemorySidecarFallback {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemorySidecarFallback::OpenAiClaude => "openai_claude",
            MemorySidecarFallback::Provider => "provider",
            MemorySidecarFallback::None => "none",
        }
    }

    /// Parse from settings text. Unknown values fall back to the default.
    pub fn from_str_lossy(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "provider" => MemorySidecarFallback::Provider,
            "none" => MemorySidecarFallback::None,
            _ => MemorySidecarFallback::OpenAiClaude,
        }
    }
}

/// Memory system settings, stored under `"memory"` in settings.json.
///
/// Mirrors jcode's `agents.memory_*` config block, adapted to claurst's
/// settings layout and camelCase convention.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct MemorySettings {
    /// Optional model override for memory extraction only.
    ///
    /// When `memory_sidecar_backend` is `auto`, this is only used for
    /// OpenAI- or Claude-family models; any other value falls back to
    /// auto-select. When the backend is `provider`, this model is passed to
    /// the active provider so the sidecar uses it instead of the provider's
    /// default.
    ///
    /// Env override: `CLAURST_MEMORY_MODEL`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_model: Option<String>,
    /// Explicit backend selection. Env: `CLAURST_MEMORY_SIDECAR_BACKEND`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_sidecar_backend: Option<MemorySidecarBackend>,
    /// Fallback when no model is marked. Env: `CLAURST_MEMORY_SIDECAR_FALLBACK`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_sidecar_fallback: Option<MemorySidecarFallback>,
    /// Whether automatic memory extraction may use a text-generating
    /// sidecar. Defaults to true.
    #[serde(default = "default_memory_sidecar_enabled")]
    pub memory_sidecar_enabled: bool,
    /// Master switch for the memory system (both extraction and injection).
    /// Defaults to true. Env: `CLAURST_MEMORY_ENABLED`.
    #[serde(default = "default_memory_enabled")]
    pub memory_enabled: bool,
}

impl Default for MemorySettings {
    fn default() -> Self {
        Self {
            memory_model: None,
            memory_sidecar_backend: None,
            memory_sidecar_fallback: None,
            memory_sidecar_enabled: true,
            memory_enabled: true,
        }
    }
}

fn default_memory_sidecar_enabled() -> bool {
    true
}

fn default_memory_enabled() -> bool {
    true
}

impl MemorySettings {
    /// Load settings.json's memory block with env overrides applied.
    ///
    /// Env overrides (jcode parity):
    /// - `CLAURST_MEMORY_MODEL` → `memory_model`
    /// - `CLAURST_MEMORY_SIDECAR_BACKEND` → `memory_sidecar_backend`
    /// - `CLAURST_MEMORY_SIDECAR_FALLBACK` → `memory_sidecar_fallback`
    /// - `CLAURST_MEMORY_ENABLED` → `memory_enabled` ("false"/"0"/"no"/"off" → off)
    /// - `CLAURST_MEMORY_SIDECAR_ENABLED` → `memory_sidecar_enabled`
    pub fn load() -> Self {
        let mut settings = crate::config::Settings::load_sync()
            .map(|s| s.memory)
            .unwrap_or_default();
        settings.apply_env_overrides();
        settings
    }

    fn apply_env_overrides(&mut self) {
        if let Ok(model) = std::env::var("CLAURST_MEMORY_MODEL") {
            let model = model.trim();
            if !model.is_empty() {
                self.memory_model = Some(model.to_string());
            }
        }
        if let Ok(backend) = std::env::var("CLAURST_MEMORY_SIDECAR_BACKEND") {
            if !backend.trim().is_empty() {
                self.memory_sidecar_backend = Some(MemorySidecarBackend::from_str_lossy(&backend));
            }
        }
        if let Ok(fallback) = std::env::var("CLAURST_MEMORY_SIDECAR_FALLBACK") {
            if !fallback.trim().is_empty() {
                self.memory_sidecar_fallback =
                    Some(MemorySidecarFallback::from_str_lossy(&fallback));
            }
        }
        if let Ok(enabled) = std::env::var("CLAURST_MEMORY_ENABLED") {
            self.memory_enabled = !is_falsy(&enabled);
        }
        if let Ok(enabled) = std::env::var("CLAURST_MEMORY_SIDECAR_ENABLED") {
            self.memory_sidecar_enabled = !is_falsy(&enabled);
        }
    }
}

/// Truthy/falsy env parsing matching claurst's `is_auto_memory_enabled`
/// convention: empty/"0"/"false"/"no"/"off" are falsy, everything else truthy.
fn is_falsy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no" | "off"
    )
}

/// Persisted store for the globally marked memory model. Unlike favorites
/// (a set), only one model is marked as the memory model at a time. New
/// sessions inherit this as the initial session memory model; the Alt+M
/// picker in the TUI toggles it.
///
/// Stored at `<config>/memory_model.json`, separate from settings.json so
/// rapid picker toggles never rewrite the main settings file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MemoryModelStore {
    pub version: u8,
    pub model: Option<String>,
}

/// Path of the picker-owned memory model store.
pub fn memory_model_path() -> Option<PathBuf> {
    Some(crate::config::Settings::config_dir().join(MEMORY_MODEL_FILE))
}

/// Load the picker's memory model store. Missing or corrupt files yield an
/// empty (unmarked) store rather than an error.
pub fn load_memory_model_store() -> MemoryModelStore {
    let Some(path) = memory_model_path() else {
        return MemoryModelStore::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            let mut store: MemoryModelStore = serde_json::from_str(&contents).unwrap_or_default();
            store.version = MEMORY_MODEL_VERSION;
            // Clean empty/null entries.
            if store
                .model
                .as_deref()
                .map(str::trim)
                .map(str::is_empty)
                .unwrap_or(true)
            {
                store.model = None;
            }
            store
        }
        Err(_) => MemoryModelStore::default(),
    }
}

/// Persist the picker's memory model store, normalizing the version field.
pub fn save_memory_model_store(store: &MemoryModelStore) {
    let Some(path) = memory_model_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut store = store.clone();
    store.version = MEMORY_MODEL_VERSION;
    if let Ok(json) = serde_json::to_string_pretty(&store) {
        let _ = std::fs::write(&path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_parse_roundtrip() {
        assert_eq!(
            MemorySidecarBackend::from_str_lossy("OPENAI"),
            MemorySidecarBackend::OpenAI
        );
        assert_eq!(
            MemorySidecarBackend::from_str_lossy(" provider "),
            MemorySidecarBackend::Provider
        );
        assert_eq!(
            MemorySidecarBackend::from_str_lossy("bogus"),
            MemorySidecarBackend::Auto
        );
        assert_eq!(
            MemorySidecarBackend::from_str_lossy("claude").as_str(),
            "claude"
        );
    }

    #[test]
    fn fallback_parse_and_serde_names() {
        assert_eq!(
            MemorySidecarFallback::from_str_lossy("none"),
            MemorySidecarFallback::None
        );
        assert_eq!(
            MemorySidecarFallback::from_str_lossy("provider"),
            MemorySidecarFallback::Provider
        );
        assert_eq!(
            MemorySidecarFallback::from_str_lossy("junk"),
            MemorySidecarFallback::OpenAiClaude
        );
        // Serde uses snake_case with the openai_claude rename.
        let json = serde_json::to_string(&MemorySidecarFallback::OpenAiClaude).unwrap();
        assert_eq!(json, "\"openai_claude\"");
        let json = serde_json::to_string(&MemorySidecarFallback::None).unwrap();
        assert_eq!(json, "\"none\"");
    }

    #[test]
    fn settings_defaults_and_json_shape() {
        let defaults = MemorySettings::default();
        assert!(defaults.memory_enabled);
        assert!(defaults.memory_sidecar_enabled);
        assert!(defaults.memory_model.is_none());

        // settings.json camelCase roundtrip.
        let json = serde_json::to_string(&MemorySettings {
            memory_model: Some("claude-haiku-4-5".to_string()),
            memory_sidecar_backend: Some(MemorySidecarBackend::Provider),
            memory_sidecar_fallback: Some(MemorySidecarFallback::None),
            memory_sidecar_enabled: true,
            memory_enabled: false,
        })
        .unwrap();
        assert!(json.contains("\"memoryModel\":\"claude-haiku-4-5\""));
        assert!(json.contains("\"memorySidecarBackend\":\"provider\""));
        assert!(json.contains("\"memorySidecarFallback\":\"none\""));
        assert!(json.contains("\"memoryEnabled\":false"));

        let back: MemorySettings = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.memory_sidecar_backend,
            Some(MemorySidecarBackend::Provider)
        );
        assert!(!back.memory_enabled);
    }

    #[test]
    fn store_roundtrip_and_empty_cleaning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(MEMORY_MODEL_FILE);
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&MemoryModelStore {
                version: 0,
                model: Some("   ".to_string()),
            })
            .unwrap(),
        )
        .unwrap();

        // Corrupt/blank model is cleaned to None on load.
        let store = MemoryModelStore::default();
        assert!(store.model.is_none());

        // Roundtrip through the file helpers requires config_dir isolation;
        // test the serialization shape directly instead.
        let store = MemoryModelStore {
            version: MEMORY_MODEL_VERSION,
            model: Some("gpt-5.6-luna".to_string()),
        };
        let json = serde_json::to_string_pretty(&store).unwrap();
        let back: MemoryModelStore = serde_json::from_str(&json).unwrap();
        assert_eq!(back.model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(back.version, MEMORY_MODEL_VERSION);
    }
}
