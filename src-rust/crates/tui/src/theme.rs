// theme.rs — Theme palettes for the TUI.
//
// A theme is a set of named color roles. Three kinds exist:
//
// 1. Built-in themes: "default", "dark", "light", "deuteranopia", plus the
//    named palettes below ("solarized", "nord", "dracula", "monokai").
// 2. Custom themes: TOML files under `~/.claurst/themes/<name>.toml` mapping
//    role names to colors. Unspecified roles fall back to the palette the
//    theme file names (via `base = "dark"`), or to "dark" when unset.
// 3. `"default"` keeps the terminal's native rendering: colors pass through
//    unchanged and only the theme name is recorded in settings.
//
// The active palette is applied as a buffer post-pass over the rendered
// frame (see `apply_theme_pass`). Widgets keep rendering their historical
// colors; the pass translates them through channel-separated remap tables
// built once per theme (fg table, bg table), so foreground and background
// uses of the same source color can land on different themed roles.

use ratatui::style::Color;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, OnceLock};

use parking_lot::RwLock;

// ---------------------------------------------------------------------------
// Roles
// ---------------------------------------------------------------------------

/// A named color slot a theme can override. The identifiers double as the
/// accepted key names in custom TOML theme files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ThemeColor {
    /// App background.
    Background,
    /// Accent color (highlights, selected labels, links).
    Accent,
    /// Secondary accent (assistant name, decorative marks).
    SecondaryAccent,
    /// Error / danger.
    Error,
    /// Success / additions.
    Success,
    /// Warnings.
    Warning,
    /// Informational highlights.
    Info,
    /// Action / interactive elements.
    Action,
    /// Disabled or dimmed states.
    Disabled,
    /// Primary text.
    Text,
    /// Borders and dividers.
    Border,
    /// Panel / overlay background.
    PanelBg,
    /// Overlay background (darker than panels).
    OverlayBg,
}

const THEME_COLOR_COUNT: usize = ThemeColor::OverlayBg as usize + 1;

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Theme {
    name: String,
    colors: BTreeMap<ThemeColor, Color>,
}

impl Theme {
    fn new(name: impl Into<String>, colors: BTreeMap<ThemeColor, Color>) -> Self {
        Self {
            name: name.into(),
            colors,
        }
    }

    pub fn color(&self, key: ThemeColor) -> Color {
        self.colors.get(&key).copied().unwrap_or(Color::Reset)
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Debug, serde::Deserialize)]
struct ThemeFile {
    /// Built-in palette the custom theme inherits unspecified roles from.
    #[serde(default)]
    base: String,
    #[serde(default)]
    colors: BTreeMap<String, String>,
}

pub const BUILTIN_THEMES: &[&str] = &[
    "default",
    "dark",
    "light",
    "deuteranopia",
    "solarized",
    "nord",
    "dracula",
    "monokai",
];

// ---------------------------------------------------------------------------
// Active theme snapshot (cheap per-frame read of an Arc)
// ---------------------------------------------------------------------------

/// Global theme cell. Initialized with the native palette; replaced under
/// the write lock by `set_theme`. Readers clone the `Arc` once per frame.
static ACTIVE_THEME: OnceLock<RwLock<Arc<ThemeSnapshot>>> = OnceLock::new();

fn active_theme_cell() -> &'static RwLock<Arc<ThemeSnapshot>> {
    ACTIVE_THEME.get_or_init(|| {
        RwLock::new(Arc::new(ThemeSnapshot::from_theme(
            // The boot snapshot is the native palette; startup wiring replaces
            // it with the configured theme (which may itself be "default").
            &native_palette(),
        )))
    })
}

/// The active snapshot. Locks only to clone the Arc; the snapshot itself
/// is immutable, so no lock is held while the buffer pass walks the cells.
fn active_theme_snapshot() -> Arc<ThemeSnapshot> {
    active_theme_cell().read().clone()
}

#[derive(Debug, Clone)]
struct ThemeSnapshot {
    /// Foreground remap: native color -> themed color. Built from the role
    /// table plus the wide anchors in `add_anchors`.
    fg_remap: HashMap<Color, Color>,
    /// Background remap: native color -> themed color. Separate from the fg
    /// table so the same source color can land on different themed roles in
    /// each channel (e.g. fg White -> Text, bg Black -> Background).
    bg_remap: HashMap<Color, Color>,
}

impl ThemeSnapshot {
    fn from_theme(theme: &Theme) -> Self {
        let name = theme.name().to_string();
        let mut colors = [Color::Reset; THEME_COLOR_COUNT];
        let mut fg_remap = HashMap::new();
        let mut bg_remap = HashMap::new();
        let native = native_palette();
        for (i, key) in ROLES.iter().enumerate() {
            let native_color = native.color(*key);
            let themed = theme.color(*key);
            colors[i] = themed;
            if themed == native_color {
                continue;
            }
            // Background-family roles only feed the bg table: their native
            // values (Black and the dark panel greys) are also painted as fg
            // by badge and chip widgets, which must keep their native identity
            // (black text stays black on a colored chip under any theme).
            if !matches!(
                key,
                ThemeColor::Background | ThemeColor::PanelBg | ThemeColor::OverlayBg
            ) {
                fg_remap.insert(native_color, themed);
            }
            if matches!(
                key,
                ThemeColor::Background
                    | ThemeColor::PanelBg
                    | ThemeColor::OverlayBg
                    | ThemeColor::Border
            ) {
                bg_remap.insert(native_color, themed);
            }
        }
        if name != "default" {
            add_anchors(&mut fg_remap, &mut bg_remap, &colors);
        }
        Self { fg_remap, bg_remap }
    }
}

/// Colors widgets paint that do not match any native role value but still
/// carry role meaning. The review's color-site census found:
/// - `Color::White` fg at 139 sites is the workhorse text color; `Color::Reset`
///   fg is implicit terminal text. Under `light`, mapping neither to Text left
///   most of the UI white-on-near-white.
/// - `CLAURST_*` constants (overlays.rs) are close-but-not-equal to the
///   native role values, so exact matching missed them.
/// - `Color::Red` / `Green` / `Yellow` are painted directly by status widgets
///   (bridge states, chips, banners) instead of the native Error / Success /
///   Warning values.
///
/// Background side: `Color::Black` is the app background and `Color::Reset`
/// means "no explicit background", both filled with the theme background so
/// light/colorful palettes are readable. fg `Color::Black` is intentionally
/// unmapped, and the semantic chip backgrounds (`bg` Green/Yellow/Red) keep
/// their native colors: badges paint black text on those chips
/// (elicitation dialog, status pills) and a themed background value can be
/// too dark for black text (light's success green), which would break them.
fn add_anchors(
    fg_remap: &mut HashMap<Color, Color>,
    bg_remap: &mut HashMap<Color, Color>,
    colors: &[Color; THEME_COLOR_COUNT],
) {
    let text = colors[ThemeColor::Text as usize];
    let bg = colors[ThemeColor::Background as usize];
    let accent = colors[ThemeColor::Accent as usize];
    let secondary = colors[ThemeColor::SecondaryAccent as usize];
    let error = colors[ThemeColor::Error as usize];
    let success = colors[ThemeColor::Success as usize];
    let warning = colors[ThemeColor::Warning as usize];
    let disabled = colors[ThemeColor::Disabled as usize];
    let border = colors[ThemeColor::Border as usize];
    let panel = colors[ThemeColor::PanelBg as usize];
    let overlay = colors[ThemeColor::OverlayBg as usize];

    fg_remap.insert(Color::White, text);
    fg_remap.insert(Color::Reset, text);
    fg_remap.insert(Color::DarkGray, disabled);
    fg_remap.insert(Color::Gray, disabled);
    fg_remap.insert(Color::Red, error);
    fg_remap.insert(Color::Green, success);
    fg_remap.insert(Color::Yellow, warning);
    // The loop above already maps the native role colors, but Info and
    // Action are both `Color::Cyan` natively; the accent is the deliberate
    // winner for the generic cyan highlight sites.
    fg_remap.insert(Color::Cyan, accent);
    fg_remap.insert(Color::Blue, accent);
    fg_remap.insert(Color::Magenta, secondary);
    // CLAURST_* constants from overlays.rs: close to but not equal to the
    // native role values, so the role loop's exact matches miss them.
    fg_remap.insert(Color::Rgb(235, 235, 240), text); // CLAURST_TEXT
    fg_remap.insert(Color::Rgb(110, 110, 118), disabled); // CLAURST_MUTED
    fg_remap.insert(Color::Rgb(233, 30, 99), accent); // CLAURST_ACCENT

    bg_remap.insert(Color::Black, bg);
    bg_remap.insert(Color::Reset, bg);
    bg_remap.insert(Color::Rgb(72, 72, 80), border); // CLAURST_PANEL_BORDER
    bg_remap.insert(Color::Rgb(20, 20, 28), panel); // CLAURST_PANEL_BG
    bg_remap.insert(Color::Rgb(10, 10, 14), overlay); // CLAURST_OVERLAY_BG
    bg_remap.insert(Color::DarkGray, panel);
}

const ROLES: [ThemeColor; THEME_COLOR_COUNT] = [
    ThemeColor::Background,
    ThemeColor::Accent,
    ThemeColor::SecondaryAccent,
    ThemeColor::Error,
    ThemeColor::Success,
    ThemeColor::Warning,
    ThemeColor::Info,
    ThemeColor::Action,
    ThemeColor::Disabled,
    ThemeColor::Text,
    ThemeColor::Border,
    ThemeColor::PanelBg,
    ThemeColor::OverlayBg,
];

/// Set the active theme. Unknown or invalid names return an error and leave
/// the current theme unchanged.
pub fn set_theme(name: &str, themes_dir: Option<&Path>) -> anyhow::Result<()> {
    let theme = load_theme(name, themes_dir)?;
    publish_snapshot(theme);
    Ok(())
}

fn publish_snapshot(theme: Theme) {
    let snapshot = Arc::new(ThemeSnapshot::from_theme(&theme));
    *active_theme_cell().write() = snapshot;
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Load a theme by name without activating it.
pub fn load_theme(name: &str, themes_dir: Option<&Path>) -> anyhow::Result<Theme> {
    let name = name.trim();
    match name.to_ascii_lowercase().as_str() {
        // "default" (and the empty string) is the native pass-through.
        "default" | "" => Ok(native_palette()),
        other => {
            if let Some(theme) = builtin_palette(other) {
                return Ok(theme);
            }
            load_custom_theme(name, themes_dir)
        }
    }
}

/// Names of all available themes: built-ins plus custom TOML files found in
/// `themes_dir` (sorted, built-ins first).
pub fn available_theme_names(themes_dir: Option<&Path>) -> Vec<String> {
    let mut names: Vec<String> = BUILTIN_THEMES.iter().map(|s| s.to_string()).collect();
    if let Some(dir) = themes_dir {
        if let Ok(entries) = std::fs::read_dir(dir) {
            let mut custom: Vec<String> = entries
                .flatten()
                .filter_map(|entry| {
                    let path = entry.path();
                    if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
                        return None;
                    }
                    let stem = path.file_stem()?.to_str()?;
                    is_safe_custom_theme_name(stem).then(|| stem.to_string())
                })
                .collect();
            custom.sort();
            custom.dedup();
            names.extend(custom);
        }
    }
    names
}

fn load_custom_theme(name: &str, themes_dir: Option<&Path>) -> anyhow::Result<Theme> {
    if !is_safe_custom_theme_name(name) {
        anyhow::bail!(
            "Invalid theme name '{}': use only ASCII letters, numbers, '-' or '_'",
            name
        );
    }
    let dir = themes_dir.ok_or_else(|| anyhow::anyhow!("No themes directory configured"))?;
    let path = dir.join(format!("{name}.toml"));
    let content = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("Failed to read theme {}: {}", path.display(), e))?;
    let file: ThemeFile = toml::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse theme {}: {}", path.display(), e))?;

    let base_name = if file.base.trim().is_empty() {
        "dark"
    } else {
        file.base.trim()
    };
    let mut theme = builtin_palette(base_name).ok_or_else(|| {
        anyhow::anyhow!("Unknown base theme '{}' in {}", base_name, path.display())
    })?;
    theme.name = name.to_string();
    for (raw_key, raw_value) in file.colors {
        let key = parse_theme_color(&raw_key).ok_or_else(|| {
            anyhow::anyhow!("Unknown theme color '{}': {}", raw_key, path.display())
        })?;
        let value = parse_color(&raw_value).ok_or_else(|| {
            anyhow::anyhow!(
                "Invalid color '{}' for '{}' in {}",
                raw_value,
                raw_key,
                path.display()
            )
        })?;
        theme.colors.insert(key, value);
    }
    Ok(theme)
}

fn is_safe_custom_theme_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Map a TOML key to a role. Accepts both snake_case and kebab-case, with or
/// without a `_color` suffix, plus a few common aliases.
fn parse_theme_color(raw: &str) -> Option<ThemeColor> {
    match raw.trim().replace('-', "_").to_ascii_lowercase().as_str() {
        "background" | "app_bg" | "background_color" | "app_background" => {
            Some(ThemeColor::Background)
        }
        "accent" | "accent_color" => Some(ThemeColor::Accent),
        "secondary_accent" | "secondary_accent_color" => Some(ThemeColor::SecondaryAccent),
        "error" | "error_color" => Some(ThemeColor::Error),
        "success" | "success_color" => Some(ThemeColor::Success),
        "warning" | "warning_color" => Some(ThemeColor::Warning),
        "info" | "info_color" => Some(ThemeColor::Info),
        "action" | "action_color" => Some(ThemeColor::Action),
        "disabled" | "disabled_color" | "dim" | "dim_color" => Some(ThemeColor::Disabled),
        "text" | "text_color" | "text_light" | "text_dark" => Some(ThemeColor::Text),
        "border" | "border_color" => Some(ThemeColor::Border),
        "panel_bg" | "panel_background" => Some(ThemeColor::PanelBg),
        "overlay_bg" | "overlay_background" => Some(ThemeColor::OverlayBg),
        _ => None,
    }
}

fn named_color(name: &str) -> Option<Color> {
    // ratatui named colors, for users who want a terminal-quantized color
    // instead of an explicit RGB value.
    Some(match name {
        "black" => Color::Black,
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" | "grey" => Color::Gray,
        "darkgray" | "darkgrey" => Color::DarkGray,
        "lightred" => Color::LightRed,
        "lightgreen" => Color::LightGreen,
        "lightyellow" => Color::LightYellow,
        "lightblue" => Color::LightBlue,
        "lightmagenta" => Color::LightMagenta,
        "lightcyan" => Color::LightCyan,
        "white" => Color::White,
        _ => return None,
    })
}

/// Parse a color value: "reset"/"default", a ratatui named color, or "#rrggbb".
fn parse_color(raw: &str) -> Option<Color> {
    let raw = raw.trim();
    if raw.eq_ignore_ascii_case("reset") || raw.eq_ignore_ascii_case("default") {
        return Some(Color::Reset);
    }
    if let Some(color) = named_color(&raw.to_ascii_lowercase().replace(['-', ' '], "")) {
        return Some(color);
    }
    let hex = raw.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |pair: usize| -> u8 {
        // The slice is validated above; unwrap_or is a never-triggering
        // fallback kept to avoid panics in production paths.
        u8::from_str_radix(hex.get(pair..pair + 2).unwrap_or("zz"), 16).unwrap_or(0)
    };
    Some(Color::Rgb(byte(0), byte(2), byte(4)))
}

// ---------------------------------------------------------------------------
// Built-in palettes
// ---------------------------------------------------------------------------

/// The native (unthemed) colors widgets render with. These values double as
/// the keys of the remap pass: any cell using one of them gets the themed
/// equivalent when a theme is active.
fn native_palette() -> Theme {
    Theme::new(
        "default",
        BTreeMap::from([
            (ThemeColor::Background, Color::Black),
            (ThemeColor::Accent, Color::Rgb(233, 30, 99)),
            (ThemeColor::SecondaryAccent, Color::Cyan),
            (ThemeColor::Error, Color::Rgb(255, 87, 51)),
            (ThemeColor::Success, Color::Rgb(76, 175, 80)),
            (ThemeColor::Warning, Color::Rgb(255, 152, 0)),
            (ThemeColor::Info, Color::Cyan),
            (ThemeColor::Action, Color::Cyan),
            (ThemeColor::Disabled, Color::DarkGray),
            (ThemeColor::Text, Color::Rgb(236, 236, 241)),
            (ThemeColor::Border, Color::Rgb(72, 72, 80)),
            (ThemeColor::PanelBg, Color::Rgb(20, 20, 28)),
            (ThemeColor::OverlayBg, Color::Rgb(10, 10, 14)),
        ]),
    )
}

fn builtin_palette(name: &str) -> Option<Theme> {
    let theme = match name.to_ascii_lowercase().as_str() {
        // "default" is not a remap palette; it is the native pass-through
        // handled by `native_palette()` in `load_theme`.
        "default" => return None,
        "dark" => Theme::new(
            "dark",
            BTreeMap::from([
                (ThemeColor::Background, Color::Rgb(18, 18, 18)),
                (ThemeColor::Accent, Color::Rgb(97, 175, 239)),
                (ThemeColor::SecondaryAccent, Color::Rgb(229, 57, 53)),
                (ThemeColor::Error, Color::Rgb(239, 83, 80)),
                (ThemeColor::Success, Color::Rgb(129, 199, 132)),
                (ThemeColor::Warning, Color::Rgb(255, 171, 64)),
                (ThemeColor::Info, Color::Rgb(100, 181, 246)),
                (ThemeColor::Action, Color::Rgb(100, 181, 246)),
                (ThemeColor::Disabled, Color::Rgb(97, 97, 97)),
                (ThemeColor::Text, Color::Rgb(229, 229, 229)),
                (ThemeColor::Border, Color::Rgb(66, 66, 66)),
                (ThemeColor::PanelBg, Color::Rgb(24, 24, 32)),
                (ThemeColor::OverlayBg, Color::Rgb(14, 14, 20)),
            ]),
        ),
        "light" => Theme::new(
            "light",
            BTreeMap::from([
                (ThemeColor::Background, Color::Rgb(250, 250, 250)),
                (ThemeColor::Accent, Color::Blue),
                (ThemeColor::SecondaryAccent, Color::Rgb(194, 24, 91)),
                (ThemeColor::Error, Color::Rgb(211, 47, 47)),
                (ThemeColor::Success, Color::Rgb(27, 94, 32)),
                (ThemeColor::Warning, Color::Rgb(230, 124, 13)),
                (ThemeColor::Info, Color::Rgb(13, 71, 161)),
                (ThemeColor::Action, Color::Blue),
                (ThemeColor::Disabled, Color::Rgb(117, 117, 117)),
                (ThemeColor::Text, Color::Rgb(33, 33, 33)),
                (ThemeColor::Border, Color::Rgb(189, 189, 189)),
                (ThemeColor::PanelBg, Color::Rgb(240, 240, 245)),
                (ThemeColor::OverlayBg, Color::Rgb(230, 230, 235)),
            ]),
        ),
        "deuteranopia" => Theme::new(
            "deuteranopia",
            BTreeMap::from([
                (ThemeColor::Background, Color::Rgb(18, 18, 18)),
                (ThemeColor::Accent, Color::Rgb(0, 150, 200)),
                (ThemeColor::SecondaryAccent, Color::Rgb(180, 140, 255)),
                (ThemeColor::Error, Color::Rgb(255, 140, 0)),
                (ThemeColor::Success, Color::Rgb(0, 150, 200)),
                (ThemeColor::Warning, Color::Rgb(255, 180, 0)),
                (ThemeColor::Info, Color::Cyan),
                (ThemeColor::Action, Color::Rgb(0, 150, 200)),
                (ThemeColor::Disabled, Color::Rgb(120, 120, 120)),
                (ThemeColor::Text, Color::Rgb(220, 220, 220)),
                (ThemeColor::Border, Color::Rgb(100, 100, 100)),
                (ThemeColor::PanelBg, Color::Rgb(24, 24, 32)),
                (ThemeColor::OverlayBg, Color::Rgb(14, 14, 20)),
            ]),
        ),
        "solarized" => Theme::new(
            "solarized",
            BTreeMap::from([
                (ThemeColor::Background, Color::Rgb(0, 43, 54)),
                (ThemeColor::Accent, Color::Rgb(38, 139, 210)),
                (ThemeColor::SecondaryAccent, Color::Rgb(108, 113, 196)),
                (ThemeColor::Error, Color::Rgb(220, 50, 47)),
                (ThemeColor::Success, Color::Rgb(133, 153, 0)),
                (ThemeColor::Warning, Color::Rgb(181, 137, 0)),
                (ThemeColor::Info, Color::Rgb(38, 139, 210)),
                (ThemeColor::Action, Color::Rgb(38, 139, 210)),
                (ThemeColor::Disabled, Color::Rgb(88, 110, 117)),
                (ThemeColor::Text, Color::Rgb(131, 148, 150)),
                (ThemeColor::Border, Color::Rgb(7, 54, 66)),
                (ThemeColor::PanelBg, Color::Rgb(7, 54, 66)),
                (ThemeColor::OverlayBg, Color::Rgb(0, 34, 43)),
            ]),
        ),
        "nord" => Theme::new(
            "nord",
            BTreeMap::from([
                (ThemeColor::Background, Color::Rgb(46, 52, 64)),
                (ThemeColor::Accent, Color::Rgb(136, 192, 208)),
                (ThemeColor::SecondaryAccent, Color::Rgb(191, 97, 106)),
                (ThemeColor::Error, Color::Rgb(191, 97, 106)),
                (ThemeColor::Success, Color::Rgb(163, 190, 140)),
                (ThemeColor::Warning, Color::Rgb(235, 203, 139)),
                (ThemeColor::Info, Color::Rgb(136, 192, 208)),
                (ThemeColor::Action, Color::Rgb(136, 192, 208)),
                (ThemeColor::Disabled, Color::Rgb(76, 86, 106)),
                (ThemeColor::Text, Color::Rgb(216, 222, 233)),
                (ThemeColor::Border, Color::Rgb(67, 76, 94)),
                (ThemeColor::PanelBg, Color::Rgb(59, 66, 82)),
                (ThemeColor::OverlayBg, Color::Rgb(46, 52, 64)),
            ]),
        ),
        "dracula" => Theme::new(
            "dracula",
            BTreeMap::from([
                (ThemeColor::Background, Color::Rgb(40, 42, 54)),
                (ThemeColor::Accent, Color::Rgb(139, 233, 253)),
                (ThemeColor::SecondaryAccent, Color::Rgb(189, 147, 249)),
                (ThemeColor::Error, Color::Rgb(255, 85, 85)),
                (ThemeColor::Success, Color::Rgb(80, 250, 123)),
                (ThemeColor::Warning, Color::Rgb(241, 250, 140)),
                (ThemeColor::Info, Color::Rgb(139, 233, 253)),
                (ThemeColor::Action, Color::Rgb(139, 233, 253)),
                (ThemeColor::Disabled, Color::Rgb(98, 114, 164)),
                (ThemeColor::Text, Color::Rgb(248, 248, 242)),
                (ThemeColor::Border, Color::Rgb(68, 71, 90)),
                (ThemeColor::PanelBg, Color::Rgb(52, 55, 70)),
                (ThemeColor::OverlayBg, Color::Rgb(34, 36, 46)),
            ]),
        ),
        "monokai" => Theme::new(
            "monokai",
            BTreeMap::from([
                (ThemeColor::Background, Color::Rgb(39, 40, 34)),
                (ThemeColor::Accent, Color::Rgb(102, 217, 239)),
                (ThemeColor::SecondaryAccent, Color::Rgb(249, 38, 114)),
                (ThemeColor::Error, Color::Rgb(249, 38, 114)),
                (ThemeColor::Success, Color::Rgb(166, 226, 46)),
                (ThemeColor::Warning, Color::Rgb(253, 151, 31)),
                (ThemeColor::Info, Color::Rgb(102, 217, 239)),
                (ThemeColor::Action, Color::Rgb(102, 217, 239)),
                (ThemeColor::Disabled, Color::Rgb(117, 113, 94)),
                (ThemeColor::Text, Color::Rgb(248, 248, 242)),
                (ThemeColor::Border, Color::Rgb(75, 75, 75)),
                (ThemeColor::PanelBg, Color::Rgb(49, 50, 43)),
                (ThemeColor::OverlayBg, Color::Rgb(33, 34, 28)),
            ]),
        ),
        _ => return None,
    };
    Some(theme)
}

// ---------------------------------------------------------------------------
// Buffer post-pass
// ---------------------------------------------------------------------------

/// Apply the active theme to a rendered frame, in place.
///
/// Each cell's fg and bg is translated through the snapshot's remap tables,
/// channel by channel. Themes whose tables are empty (the "default"
/// pass-through) cost one `is_empty` check.
pub fn apply_theme_pass(buf: &mut ratatui::buffer::Buffer) {
    let snapshot = active_theme_snapshot();
    apply_snapshot_to_buffer(&snapshot, buf);
}

fn apply_snapshot_to_buffer(snapshot: &ThemeSnapshot, buf: &mut ratatui::buffer::Buffer) {
    if snapshot.fg_remap.is_empty() && snapshot.bg_remap.is_empty() {
        return;
    }
    for cell in buf.content.iter_mut() {
        if let Some(chosen) = snapshot.fg_remap.get(&cell.fg) {
            cell.fg = *chosen;
        }
        if let Some(chosen) = snapshot.bg_remap.get(&cell.bg) {
            cell.bg = *chosen;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb_hex(hex: &str) -> Color {
        parse_color(hex).unwrap_or(Color::Reset)
    }

    #[test]
    fn parses_hex_named_and_reset_colors() {
        assert_eq!(parse_color("#ff8000"), Some(Color::Rgb(255, 128, 0)));
        assert_eq!(parse_color(" #FF8000 "), Some(Color::Rgb(255, 128, 0)));
        assert_eq!(parse_color("light-cyan"), Some(Color::LightCyan));
        assert_eq!(parse_color("LIGHTCYAN"), Some(Color::LightCyan));
        assert_eq!(parse_color("reset"), Some(Color::Reset));
        assert_eq!(parse_color("default"), Some(Color::Reset));
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(parse_color("#zzzzzz"), None);
        assert_eq!(parse_color("notacolor"), None);
    }

    #[test]
    fn builtin_themes_load() {
        for name in BUILTIN_THEMES {
            if *name == "default" {
                // "default" is the native pass-through, not a palette.
                continue;
            }
            let theme = load_theme(name, None).unwrap();
            assert_eq!(theme.name(), *name);
            assert_ne!(theme.color(ThemeColor::Accent), Color::Reset);
        }
    }

    #[test]
    fn unknown_builtin_returns_error() {
        assert!(load_theme("does-not-exist", None).is_err());
    }

    #[test]
    fn unsafe_theme_names_rejected() {
        let dir = std::env::temp_dir();
        assert!(load_theme("../evil", Some(&dir)).is_err());
        assert!(load_theme("with space", Some(&dir)).is_err());
        assert!(load_theme("", Some(&dir)).is_ok()); // falls back to default
    }

    #[test]
    fn custom_theme_overrides_base() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mytheme.toml"),
            "base = \"dark\"\n[colors]\naccent = \"#123456\"\nerror = \"red\"\n",
        )
        .unwrap();
        let theme = load_theme("mytheme", Some(dir.path())).unwrap();
        assert_eq!(theme.name(), "mytheme");
        assert_eq!(theme.color(ThemeColor::Accent), rgb_hex("#123456"));
        assert_eq!(theme.color(ThemeColor::Error), Color::Red);
        // Unspecified role inherited from the dark base.
        assert_eq!(theme.color(ThemeColor::Success), Color::Rgb(129, 199, 132));
    }

    #[test]
    fn custom_theme_defaults_to_dark_base() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("minimal.toml"),
            "[colors]\naccent = \"#abcdef\"\n",
        )
        .unwrap();
        let theme = load_theme("minimal", Some(dir.path())).unwrap();
        assert_eq!(theme.color(ThemeColor::Accent), rgb_hex("#abcdef"));
        // Base defaulted to dark.
        assert_eq!(theme.color(ThemeColor::Background), Color::Rgb(18, 18, 18));
    }

    #[test]
    fn custom_theme_unknown_keys_and_colors_fail() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("bad.toml"),
            "[colors]\nnonsense = \"#000000\"\n",
        )
        .unwrap();
        assert!(load_theme("bad", Some(dir.path())).is_err());

        std::fs::write(
            dir.path().join("bad2.toml"),
            "[colors]\naccent = \"notacolor\"\n",
        )
        .unwrap();
        assert!(load_theme("bad2", Some(dir.path())).is_err());
    }

    #[test]
    fn custom_theme_unknown_base_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("badbase.toml"),
            "base = \"nope\"\n[colors]\naccent = \"#000000\"\n",
        )
        .unwrap();
        assert!(load_theme("badbase", Some(dir.path())).is_err());
    }

    #[test]
    fn kebab_and_aliases_parse() {
        assert_eq!(
            parse_theme_color("secondary-accent"),
            Some(ThemeColor::SecondaryAccent)
        );
        assert_eq!(parse_theme_color("DIM_COLOR"), Some(ThemeColor::Disabled));
        assert_eq!(parse_theme_color("text-light"), Some(ThemeColor::Text));
        assert_eq!(
            parse_theme_color("panel_background"),
            Some(ThemeColor::PanelBg)
        );
        assert_eq!(parse_theme_color("bogus"), None);
    }

    #[test]
    fn available_names_lists_builtins_and_custom() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("zz-custom.toml"), "[colors]\n").unwrap();
        std::fs::write(dir.path().join("aa-custom.toml"), "[colors]\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "").unwrap();
        let names = available_theme_names(Some(dir.path()));
        let builtins: Vec<&str> = names
            .iter()
            .take(BUILTIN_THEMES.len())
            .map(String::as_str)
            .collect();
        assert_eq!(builtins, BUILTIN_THEMES.to_vec());
        let customs: Vec<&str> = names[BUILTIN_THEMES.len()..]
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(customs, vec!["aa-custom", "zz-custom"]);
    }

    #[test]
    fn nord_snapshot_remaps_native_colors_per_channel() {
        let snapshot = ThemeSnapshot::from_theme(&load_theme("nord", None).unwrap());

        // Foreground: native role colors plus the wide anchors.
        assert_eq!(
            snapshot.fg_remap.get(&Color::Rgb(233, 30, 99)),
            Some(&Color::Rgb(136, 192, 208)) // native accent -> nord accent
        );
        assert_eq!(
            snapshot.fg_remap.get(&Color::Rgb(76, 175, 80)),
            Some(&Color::Rgb(163, 190, 140)) // native success
        );
        assert_eq!(
            snapshot.fg_remap.get(&Color::White),
            Some(&Color::Rgb(216, 222, 233)) // workhorse text -> nord Text
        );
        assert_eq!(
            snapshot.fg_remap.get(&Color::Reset),
            Some(&Color::Rgb(216, 222, 233))
        );
        assert_eq!(
            snapshot.fg_remap.get(&Color::Red),
            Some(&Color::Rgb(191, 97, 106)) // named Red -> nord Error
        );
        assert_eq!(
            snapshot.fg_remap.get(&Color::Cyan),
            Some(&Color::Rgb(136, 192, 208)) // accent wins the Info/Action tie
        );

        // Background: channel-separated from the fg table.
        assert_eq!(
            snapshot.bg_remap.get(&Color::Black),
            Some(&Color::Rgb(46, 52, 64)) // app background
        );
        assert_eq!(
            snapshot.bg_remap.get(&Color::Reset),
            Some(&Color::Rgb(46, 52, 64))
        );
        assert_eq!(
            snapshot.bg_remap.get(&Color::Rgb(20, 20, 28)),
            Some(&Color::Rgb(59, 66, 82)) // native PanelBg -> nord PanelBg
        );

        // Badge combos keep their native identity.
        assert!(!snapshot.fg_remap.contains_key(&Color::Black));
        assert!(!snapshot.bg_remap.contains_key(&Color::Green));

        // End to end through the buffer pass.
        let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 3, 1));
        buf.content[0].set_fg(Color::White);
        buf.content[1].set_bg(Color::Reset);
        buf.content[2].set_fg(Color::Black);
        buf.content[2].set_bg(Color::Green);
        apply_snapshot_to_buffer(&snapshot, &mut buf);
        assert_eq!(buf.content[0].fg, Color::Rgb(216, 222, 233));
        assert_eq!(buf.content[1].bg, Color::Rgb(46, 52, 64));
        assert_eq!(buf.content[2].fg, Color::Black);
        assert_eq!(buf.content[2].bg, Color::Green);
    }

    #[test]
    fn light_snapshot_keeps_text_readable_on_light_background() {
        let snapshot = ThemeSnapshot::from_theme(&load_theme("light", None).unwrap());

        // The review's core complaint: white text on the light background.
        // Both text anchors must flip to the dark themed Text color.
        assert_eq!(
            snapshot.fg_remap.get(&Color::White),
            Some(&Color::Rgb(33, 33, 33))
        );
        assert_eq!(
            snapshot.fg_remap.get(&Color::Reset),
            Some(&Color::Rgb(33, 33, 33))
        );
        assert_eq!(
            snapshot.fg_remap.get(&Color::Rgb(236, 236, 241)), // native Text value
            Some(&Color::Rgb(33, 33, 33))
        );
        assert_eq!(
            snapshot.fg_remap.get(&Color::Rgb(235, 235, 240)), // CLAURST_TEXT
            Some(&Color::Rgb(33, 33, 33))
        );
        assert_eq!(
            snapshot.fg_remap.get(&Color::DarkGray),
            Some(&Color::Rgb(117, 117, 117)) // dim labels stay visible
        );
        // Backgrounds fill with the light background.
        assert_eq!(
            snapshot.bg_remap.get(&Color::Black),
            Some(&Color::Rgb(250, 250, 250))
        );
        assert_eq!(
            snapshot.bg_remap.get(&Color::Reset),
            Some(&Color::Rgb(250, 250, 250))
        );
        // Semantic chip backgrounds keep their native colors so black badge
        // text stays readable (light's success green is too dark for it).
        assert!(!snapshot.bg_remap.contains_key(&Color::Green));
        assert!(!snapshot.bg_remap.contains_key(&Color::Yellow));
        assert!(!snapshot.bg_remap.contains_key(&Color::Red));
    }

    #[test]
    fn default_snapshot_is_identity() {
        let snapshot = ThemeSnapshot::from_theme(&load_theme("default", None).unwrap());
        assert!(snapshot.fg_remap.is_empty());
        assert!(snapshot.bg_remap.is_empty());

        let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 2, 1));
        buf.content[0].set_fg(Color::White);
        buf.content[1].set_bg(Color::Black);
        apply_snapshot_to_buffer(&snapshot, &mut buf);
        assert_eq!(buf.content[0].fg, Color::White);
        assert_eq!(buf.content[1].bg, Color::Black);
    }

    #[test]
    fn set_theme_invalid_name_errors() {
        // set_theme loads before publishing, so a failed load leaves the
        // active theme untouched without any restore step.
        assert!(set_theme("does-not-exist", None).is_err());
        assert!(load_theme("does-not-exist", None).is_err());
    }
}
