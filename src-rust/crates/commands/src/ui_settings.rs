// UI settings helpers (stored in ~/.claurst/ui-settings.json).
//
// These hold state not present in the core Config struct. Shared by many
// commands; extracted from lib.rs (issue #232). Behavior-preserving move.

// ---------------------------------------------------------------------------
// UI settings helpers (stored in ~/.claurst/ui-settings.json)
// These hold things not present in the core Config struct.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub(crate) struct UiSettings {
    #[serde(default)]
    pub editor_mode: Option<String>,       // "vim" or "normal"
    #[serde(default)]
    pub fast_mode: Option<bool>,
    #[serde(default)]
    pub voice_enabled: Option<bool>,
    #[serde(default)]
    pub statusline_show_cost: Option<bool>,
    #[serde(default)]
    pub statusline_show_tokens: Option<bool>,
    #[serde(default)]
    pub statusline_show_model: Option<bool>,
    #[serde(default)]
    pub statusline_show_time: Option<bool>,
    #[serde(default)]
    pub prompt_color: Option<String>,
    #[serde(default)]
    pub sandbox_mode: Option<bool>,
    /// Shell command patterns excluded from sandboxing (glob-style strings).
    /// Mirrors TS `excludedCommands` in settings.local.json.
    #[serde(default)]
    pub sandbox_excluded_commands: Vec<String>,
}

pub(crate) fn ui_settings_path() -> std::path::PathBuf {
    claurst_core::config::Settings::config_dir().join("ui-settings.json")
}

pub(crate) fn load_ui_settings() -> UiSettings {
    let path = ui_settings_path();
    if !path.exists() {
        return UiSettings::default();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub(crate) fn save_ui_settings(settings: &UiSettings) -> anyhow::Result<()> {
    let path = ui_settings_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(settings)?;
    std::fs::write(&path, json)?;
    Ok(())
}

pub(crate) fn mutate_ui_settings<F>(f: F) -> anyhow::Result<UiSettings>
where
    F: FnOnce(&mut UiSettings),
{
    let mut s = load_ui_settings();
    f(&mut s);
    save_ui_settings(&s)?;
    Ok(s)
}
