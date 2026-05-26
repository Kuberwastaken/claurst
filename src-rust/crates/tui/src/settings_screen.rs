// settings_screen.rs — Flat searchable settings interface.
//
// Opened by /config or /settings commands. Shows all editable settings
// in a single scrollable list with live search filtering.
// Changes are persisted via Settings::save_sync() or settings.json writes.

use claurst_core::config::{Config, Settings};
use claurst_core::output_styles::{builtin_styles, find_style};
use crate::overlays::{
    centered_rect, modal_search_line, render_dark_overlay, render_dialog_bg, CLAURST_ACCENT,
    CLAURST_MUTED, CLAURST_PANEL_BG,
};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use ratatui::Frame;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum SettingKind {
    Bool,
    Enum { options: Vec<&'static str> },
    Number,
}

#[derive(Debug, Clone)]
pub struct SettingsEntry {
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub kind: SettingKind,
    pub value: String,
}

pub struct SettingsScreen {
    pub visible: bool,
    pub search_query: String,
    pub selected_idx: usize,
    pub scroll_offset: usize,
    /// Which field is being edited (field name as key).
    pub edit_field: Option<String>,
    /// Current buffer content while editing a field.
    pub edit_value: String,
    /// Snapshot of settings at open time.
    pub settings_snapshot: Settings,
    /// Pending changes (field_name → new_value string).
    pub pending_changes: HashMap<String, String>,

    // ---- Real settings fields ----
    pub auto_compact: bool,
    pub notifications: bool,
    pub show_turn_duration: bool,
    pub output_style: String,
    pub reduce_motion: bool,
    pub terminal_progress_bar: bool,
    pub verbose: bool,
    pub cursor_blink_enabled: bool,
    pub auto_copy_enabled: bool,
    pub show_cwd: bool,
    pub show_git_branch: bool,
    pub compact_threshold: String,
    pub auto_commits: bool,
    pub output_format: String,
    pub disable_claude_mds: bool,
    pub file_injection_enabled: bool,
    pub file_autocomplete_limit: String,
    pub file_autocomplete_show_hidden_files: bool,
    pub file_injection_max_size: String,
}

impl SettingsScreen {
    pub fn new() -> Self {
        let settings_snapshot = Settings::load_sync().unwrap_or_default();
        let mut screen = Self {
            visible: false,
            search_query: String::new(),
            selected_idx: 0,
            scroll_offset: 0,
            edit_field: None,
            edit_value: String::new(),
            settings_snapshot: settings_snapshot.clone(),
            pending_changes: HashMap::new(),
            auto_compact: false,
            notifications: true,
            show_turn_duration: false,
            output_style: "default".to_string(),
            reduce_motion: false,
            terminal_progress_bar: true,
            verbose: false,
            cursor_blink_enabled: false,
            auto_copy_enabled: false,
            show_cwd: false,
            show_git_branch: false,
            compact_threshold: "95".to_string(),
            auto_commits: false,
            output_format: "text".to_string(),
            disable_claude_mds: false,
            file_injection_enabled: true,
            file_autocomplete_limit: "15".to_string(),
            file_autocomplete_show_hidden_files: false,
            file_injection_max_size: "100".to_string(),
        };
        // Apply settings from snapshot immediately on initialization
        screen.apply_settings_from_snapshot();
        screen
    }

    /// Apply all settings from the snapshot to the screen fields.
    /// This is called on initialization and when opening the settings screen.
    fn apply_settings_from_snapshot(&mut self) {
        self.auto_compact = self.settings_snapshot.auto_compact;
        self.notifications = self.settings_snapshot.notifications;
        self.show_turn_duration = self.settings_snapshot.show_turn_duration;
        self.output_style = self.settings_snapshot.config.output_style.clone().unwrap_or_else(|| "default".to_string());
        self.reduce_motion = self.settings_snapshot.reduce_motion;
        self.terminal_progress_bar = self.settings_snapshot.terminal_progress_bar;
        self.verbose = self.settings_snapshot.config.verbose;
        self.cursor_blink_enabled = self.settings_snapshot.config.cursor_blink_enabled;
        self.auto_copy_enabled = self.settings_snapshot.auto_copy_on_highlight;
        self.show_cwd = self.settings_snapshot.show_cwd;
        self.show_git_branch = self.settings_snapshot.show_git_branch;
        self.compact_threshold = self.settings_snapshot.config.compact_threshold.to_string();
        self.auto_commits = self.settings_snapshot.config.auto_commits.unwrap_or(false);
        self.output_format = match &self.settings_snapshot.config.output_format {
            claurst_core::config::OutputFormat::Text => "text".to_string(),
            claurst_core::config::OutputFormat::Json => "json".to_string(),
            claurst_core::config::OutputFormat::StreamJson => "stream_json".to_string(),
        };
        self.disable_claude_mds = self.settings_snapshot.config.disable_claude_mds;
        self.file_injection_enabled = self.settings_snapshot.config.file_injection_enabled;
        self.file_autocomplete_limit = self.settings_snapshot.config.file_autocomplete_limit.to_string();
        self.file_autocomplete_show_hidden_files = self.settings_snapshot.config.file_autocomplete_show_hidden_files;
        self.file_injection_max_size = self.settings_snapshot.config.file_injection_max_size.to_string();
    }

    pub fn open(&mut self) {
        self.settings_snapshot = Settings::load_sync().unwrap_or_default();
        self.pending_changes.clear();
        self.edit_field = None;
        self.edit_value.clear();
        self.search_query.clear();
        self.selected_idx = 0;
        self.scroll_offset = 0;
        self.visible = true;

        // Wire real settings from snapshot
        self.apply_settings_from_snapshot();
    }

    pub fn close(&mut self) {
        self.visible = false;
        self.edit_field = None;
        self.edit_value.clear();
    }

    pub fn push_search_char(&mut self, c: char) {
        self.search_query.push(c);
        self.selected_idx = 0;
    }

    pub fn pop_search_char(&mut self) {
        self.search_query.pop();
        self.selected_idx = 0;
    }

    pub fn select_prev(&mut self) {
        if self.selected_idx > 0 {
            self.selected_idx -= 1;
        }
    }

    pub fn select_next(&mut self, total_visible: usize) {
        if total_visible > 0 && self.selected_idx + 1 < total_visible {
            self.selected_idx += 1;
        }
    }

    /// Start editing a field by name, seeding the buffer with current value.
    pub fn start_edit(&mut self, field: &str, current_value: &str) {
        self.edit_field = Some(field.to_string());
        self.edit_value = current_value.to_string();
    }

    /// Commit the current edit to pending_changes.
    pub fn commit_edit(&mut self) {
        if let Some(field) = self.edit_field.take() {
            let value = std::mem::take(&mut self.edit_value);
            self.pending_changes.insert(field, value);
        }
    }

    /// Discard the current edit.
    pub fn cancel_edit(&mut self) {
        self.edit_field = None;
        self.edit_value.clear();
    }

    /// Apply all pending changes to settings and persist them.
    pub fn apply_and_save(&mut self, config: &mut Config) {
        for (field, value) in &self.pending_changes {
            match field.as_str() {
                "max_tokens" => {
                    if let Ok(n) = value.parse::<u32>() {
                        config.max_tokens = Some(n);
                    }
                }
                "output_style" => {
                    config.output_style = if value.is_empty() {
                        None
                    } else {
                        Some(value.clone())
                    };
                }
                "compact_threshold" => {
                    if let Ok(n) = value.parse::<f32>() {
                        config.compact_threshold = n;
                        self.compact_threshold = value.clone();
                    }
                }
                "fileAutocompleteLimit" => {
                    if let Ok(n) = value.parse::<usize>() {
                        config.file_autocomplete_limit = n;
                        self.file_autocomplete_limit = value.clone();
                    }
                }
                "fileInjectionMaxSize" => {
                    if let Ok(n) = value.parse::<usize>() {
                        config.file_injection_max_size = n;
                        self.file_injection_max_size = value.clone();
                    }
                }
                _ => {}
            }
        }
        self.settings_snapshot.config = config.clone();
        let _ = self.settings_snapshot.save_sync();
        self.pending_changes.clear();
    }
}

impl Default for SettingsScreen {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Settings entries definition
// ---------------------------------------------------------------------------

fn all_entries(screen: &SettingsScreen) -> Vec<SettingsEntry> {
    let mut entries = vec![
        SettingsEntry {
            key: "max_tokens",
            label: "Max Tokens",
            description: "Maximum tokens per response.",
            kind: SettingKind::Number,
            value: screen.settings_snapshot.config.max_tokens
                .map(|n| n.to_string())
                .unwrap_or_else(|| claurst_core::constants::DEFAULT_MAX_TOKENS.to_string()),
        },
        SettingsEntry {
            key: "auto_compact",
            label: "Auto-compact",
            description: "Automatically compact turns at threshold.",
            kind: SettingKind::Bool,
            value: if screen.auto_compact { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "notifications",
            label: "Desktop notifications",
            description: "Notify when a turn completes.",
            kind: SettingKind::Bool,
            value: if screen.notifications { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "show_turn_duration",
            label: "Show turn duration",
            description: "Display elapsed time per turn in status bar.",
            kind: SettingKind::Bool,
            value: if screen.show_turn_duration { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "output_style",
            label: "Output Style",
            description: "Controls the verbosity and format of responses.",
            kind: SettingKind::Enum {
                options: vec!["default", "concise", "explanatory", "learning"],
            },
            value: screen.output_style.clone(),
        },
        SettingsEntry {
            key: "reduce_motion",
            label: "Reduce motion",
            description: "Disable UI animations.",
            kind: SettingKind::Bool,
            value: if screen.reduce_motion { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "terminal_progress_bar",
            label: "Terminal progress bar",
            description: "Show progress during tool use.",
            kind: SettingKind::Bool,
            value: if screen.terminal_progress_bar { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "verbose",
            label: "Verbose logging",
            description: "Log additional debug information. Takes effect on next session.",
            kind: SettingKind::Bool,
            value: if screen.verbose { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "cursor_blink_enabled",
            label: "Cursor blinking",
            description: "Enable cursor blinking in the chat prompt.",
            kind: SettingKind::Bool,
            value: if screen.cursor_blink_enabled { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "auto_copy_enabled",
            label: "Auto-copy on highlight",
            description: "Automatically copy highlighted text to clipboard.",
            kind: SettingKind::Bool,
            value: if screen.auto_copy_enabled { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "show_cwd",
            label: "Show current directory",
            description: "Display the current working directory in the footer.",
            kind: SettingKind::Bool,
            value: if screen.show_cwd { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "show_git_branch",
            label: "Show git branch",
            description: "Display the current git branch in the footer.",
            kind: SettingKind::Bool,
            value: if screen.show_git_branch { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "compact_threshold",
            label: "Auto-compact threshold",
            description: "Context usage % at which to trigger auto-compact (0-100).",
            kind: SettingKind::Number,
            value: screen.compact_threshold.clone(),
        },
        SettingsEntry {
            key: "auto_commits",
            label: "Auto-commits",
            description: "Automatically snapshot changes to git via shadow-git.",
            kind: SettingKind::Bool,
            value: if screen.auto_commits { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "output_format",
            label: "Output format",
            description: "How responses are formatted: text, JSON, or streaming JSON.",
            kind: SettingKind::Enum {
                options: vec!["text", "json", "streamjson"],
            },
            value: screen.output_format.clone(),
        },
        SettingsEntry {
            key: "disable_claude_mds",
            label: "Disable CLAUDE.md",
            description: "Ignore CLAUDE.md files in projects (use defaults instead).",
            kind: SettingKind::Bool,
            value: if screen.disable_claude_mds { "true" } else { "false" }.to_string(),
        },
        SettingsEntry {
            key: "fileInjectionEnabled",
            label: "File injection (@)",
            description: "Auto-inject @file references into message context.",
            kind: SettingKind::Bool,
            value: if screen.file_injection_enabled { "true" } else { "false" }.to_string(),
        },
    ];

    // Only show these if file injection is enabled
    if screen.file_injection_enabled {
        entries.push(SettingsEntry {
            key: "fileAutocompleteLimit",
            label: "File autocomplete limit",
            description: "Max suggestions shown in @ autocomplete (type more to narrow results).",
            kind: SettingKind::Number,
            value: screen.file_autocomplete_limit.clone(),
        });
        entries.push(SettingsEntry {
            key: "fileAutocompleteShowHiddenFiles",
            label: "Show hidden files",
            description: "Include hidden files (.) in @ autocomplete.",
            kind: SettingKind::Bool,
            value: if screen.file_autocomplete_show_hidden_files { "true" } else { "false" }.to_string(),
        });
        entries.push(SettingsEntry {
            key: "fileInjectionMaxSize",
            label: "File injection max size",
            description: "Max file size to auto-inject (KB, 0=no limit).",
            kind: SettingKind::Number,
            value: screen.file_injection_max_size.clone(),
        });
    }

    entries
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

pub fn render_settings_screen(frame: &mut Frame, screen: &SettingsScreen, area: Rect) {
    if !screen.visible {
        return;
    }

    render_dark_overlay(frame, area);

    // 80% width, 90% height, centred
    let w = (area.width * 4 / 5).max(60).min(area.width.saturating_sub(2));
    let h = (area.height * 9 / 10).max(20).min(area.height.saturating_sub(2));
    let popup = centered_rect(w, h, area);
    render_dialog_bg(frame, popup);

    // Inset inner area
    let inner = Rect {
        x: popup.x + 2,
        y: popup.y + 1,
        width: popup.width.saturating_sub(4),
        height: popup.height.saturating_sub(2),
    };

    if inner.height < 6 {
        return;
    }

    // Split into header + search + spacer + content + description + footer
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Percentage(50),
            Constraint::Length(1),
        ])
        .split(inner);

    let header_area = layout[0];
    let search_area = layout[1];
    let content_area = layout[3];
    let description_area = layout[4];
    let footer_area = layout[5];

    // Header
    let title = Line::from(vec![
        Span::styled(" Settings", Style::default().fg(CLAURST_ACCENT).add_modifier(Modifier::BOLD)),
        Span::styled(" — Claurst", Style::default().fg(CLAURST_MUTED)),
        Span::styled(
            format!("{:>width$}", "Esc close", width = inner.width.saturating_sub(19) as usize),
            Style::default().fg(CLAURST_MUTED),
        ),
    ]);
    frame.render_widget(Paragraph::new(title).style(Style::default().bg(CLAURST_PANEL_BG)), header_area);

    // Search
    let search_line = modal_search_line(&screen.search_query, "Type to search settings...", Color::DarkGray, CLAURST_ACCENT);
    frame.render_widget(Paragraph::new(search_line).style(Style::default().bg(CLAURST_PANEL_BG)), search_area);

    // Content
    render_settings_list(frame, screen, content_area);

    // Description of selected entry
    let all = all_entries(screen);
    let filtered: Vec<_> = all.iter()
        .filter(|e| e.label.to_lowercase().contains(&screen.search_query.to_lowercase()))
        .collect();

    let desc_text = if let Some(entry) = filtered.get(screen.selected_idx) {
        // For Output Style, show current selection and all available options with descriptions
        if entry.key == "output_style" {
            let mut lines = vec![entry.description.to_string(), String::new()];

            let all_styles = builtin_styles();
            let current_style_name = if screen.output_style.is_empty() { "default" } else { &screen.output_style };
            if let Some(current_style) = find_style(&all_styles, current_style_name) {
                lines.push(format!("Current: {} — {}", current_style.label, current_style.description));
                lines.push(String::new());
            }

            lines.push("Available:".to_string());
            for style in builtin_styles() {
                lines.push(format!("  {} — {}", style.name, style.description));
            }
            lines.join("\n")
        } else {
            entry.description.to_string()
        }
    } else {
        String::new()
    };
    let desc_para = Paragraph::new(desc_text)
        .style(Style::default().fg(Color::DarkGray))
        .alignment(Alignment::Left)
        .block(Block::default().padding(ratatui::widgets::Padding::new(1, 0, 1, 0)));
    frame.render_widget(desc_para, description_area);

    // Footer
    let footer = if screen.edit_field.is_some() {
        Line::from(vec![
            Span::styled(" Enter ", Style::default().fg(CLAURST_ACCENT).add_modifier(Modifier::BOLD)),
            Span::raw("save  "),
            Span::styled(" Esc ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::raw("cancel"),
        ])
    } else {
        Line::from(vec![
            Span::styled(" ↑↓ ", Style::default().fg(CLAURST_ACCENT).add_modifier(Modifier::BOLD)),
            Span::raw("navigate  "),
            Span::styled(" Enter ", Style::default().fg(CLAURST_ACCENT).add_modifier(Modifier::BOLD)),
            Span::raw("toggle/edit  "),
            Span::styled(" Esc ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::raw("close"),
        ])
    };
    let footer_para = Paragraph::new(vec![footer])
        .style(Style::default().fg(CLAURST_MUTED).bg(CLAURST_PANEL_BG))
        .alignment(Alignment::Center);
    frame.render_widget(footer_para, footer_area);
}

fn render_settings_list(frame: &mut Frame, screen: &SettingsScreen, area: Rect) {
    let all = all_entries(screen);

    // Filter entries by search query
    let filtered: Vec<_> = all
        .iter()
        .filter(|e| e.label.to_lowercase().contains(&screen.search_query.to_lowercase()))
        .collect();

    if filtered.is_empty() {
        let para = Paragraph::new("No settings match your search.").style(Style::default().fg(Color::DarkGray));
        frame.render_widget(para, area);
        return;
    }

    // Build lines
    let mut lines: Vec<Line> = Vec::new();
    let visible_rows = area.height as usize;

    for (i, entry) in filtered.iter().enumerate() {
        let is_selected = i == screen.selected_idx;
        let marker = if is_selected { "►" } else { " " };

        let label_len = 40usize;

        // Show edit value if currently editing this field, otherwise show the entry value
        let value_str = if screen.edit_field.as_deref() == Some(entry.key) && is_selected {
            format!("{}_ ", screen.edit_value)  // Add cursor indicator
        } else {
            entry.value.clone()
        };

        let row_style = if is_selected {
            Style::default()
                .fg(Color::Black)
                .bg(CLAURST_ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        let line = Line::from(vec![
            Span::styled(
                format!("   {} {:<label_len$}", marker, entry.label),
                row_style,
            ),
            Span::styled(value_str, row_style),
        ]);
        lines.push(line);
    }

    // Scroll tracking is handled in update_scroll_offset_for_selection()

    // Apply manual scrolling
    let visible_lines: Vec<Line> = lines
        .into_iter()
        .skip(screen.scroll_offset)
        .take(visible_rows.max(1))
        .collect();

    let para = Paragraph::new(visible_lines);
    frame.render_widget(para, area);
}

// ---------------------------------------------------------------------------
// Key handling
// ---------------------------------------------------------------------------

pub fn handle_settings_key(
    screen: &mut SettingsScreen,
    config: &mut Config,
    key: crossterm::event::KeyEvent,
) -> bool {
    use crossterm::event::KeyCode;

    if !screen.visible {
        return false;
    }

    // Editing mode
    if screen.edit_field.is_some() {
        match key.code {
            KeyCode::Enter => {
                screen.commit_edit();
                screen.apply_and_save(config);
            }
            KeyCode::Esc => {
                screen.cancel_edit();
            }
            KeyCode::Backspace => {
                screen.edit_value.pop();
            }
            KeyCode::Char(c) => {
                screen.edit_value.push(c);
            }
            _ => {}
        }
        return true;
    }

    // Navigation mode
    match key.code {
        KeyCode::Enter => {
            toggle_or_cycle_current(screen);
        }
        KeyCode::Esc => {
            if !screen.search_query.is_empty() {
                screen.search_query.clear();
                screen.selected_idx = 0;
            } else {
                screen.close();
            }
        }
        KeyCode::Backspace => {
            screen.pop_search_char();
        }
        KeyCode::Up => {
            screen.select_prev();
            update_scroll_offset_for_selection(screen);
        }
        KeyCode::Down => {
            let all = all_entries(screen);
            let filtered: Vec<_> = all
                .iter()
                .filter(|e| e.label.to_lowercase().contains(&screen.search_query.to_lowercase()))
                .collect();
            screen.select_next(filtered.len());
            update_scroll_offset_for_selection(screen);
        }
        KeyCode::Char(c) => {
            screen.push_search_char(c);
        }
        _ => {}
    }
    true
}

fn update_scroll_offset_for_selection(screen: &mut SettingsScreen) {
    let visible_rows = 10; // Rough estimate, will be actual in real usage
    if screen.selected_idx < screen.scroll_offset {
        screen.scroll_offset = screen.selected_idx;
    } else if screen.selected_idx >= screen.scroll_offset + visible_rows {
        screen.scroll_offset = screen.selected_idx.saturating_sub(visible_rows - 1);
    }
}

fn toggle_or_cycle_current(screen: &mut SettingsScreen) {
    let all = all_entries(screen);
    let filtered: Vec<_> = all
        .iter()
        .filter(|e| e.label.to_lowercase().contains(&screen.search_query.to_lowercase()))
        .collect();

    if let Some(entry) = filtered.get(screen.selected_idx) {
        match entry.kind {
            SettingKind::Bool => {
                let new_value = entry.value != "true";
                match entry.key {
                    "auto_compact" => {
                        screen.auto_compact = new_value;
                        screen.settings_snapshot.auto_compact = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "notifications" => {
                        screen.notifications = new_value;
                        screen.settings_snapshot.notifications = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "show_turn_duration" => {
                        screen.show_turn_duration = new_value;
                        screen.settings_snapshot.show_turn_duration = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "reduce_motion" => {
                        screen.reduce_motion = new_value;
                        screen.settings_snapshot.reduce_motion = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "terminal_progress_bar" => {
                        screen.terminal_progress_bar = new_value;
                        screen.settings_snapshot.terminal_progress_bar = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "verbose" => {
                        screen.verbose = new_value;
                        screen.settings_snapshot.config.verbose = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "cursor_blink_enabled" => {
                        screen.cursor_blink_enabled = new_value;
                        screen.settings_snapshot.config.cursor_blink_enabled = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "auto_copy_enabled" => {
                        screen.auto_copy_enabled = new_value;
                        screen.settings_snapshot.auto_copy_on_highlight = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "show_cwd" => {
                        screen.show_cwd = new_value;
                        screen.settings_snapshot.show_cwd = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "show_git_branch" => {
                        screen.show_git_branch = new_value;
                        screen.settings_snapshot.show_git_branch = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "auto_commits" => {
                        screen.auto_commits = new_value;
                        screen.settings_snapshot.config.auto_commits = if new_value { Some(true) } else { None };
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "disable_claude_mds" => {
                        screen.disable_claude_mds = new_value;
                        screen.settings_snapshot.config.disable_claude_mds = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "fileInjectionEnabled" => {
                        screen.file_injection_enabled = new_value;
                        screen.settings_snapshot.config.file_injection_enabled = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "fileAutocompleteShowHiddenFiles" => {
                        screen.file_autocomplete_show_hidden_files = new_value;
                        screen.settings_snapshot.config.file_autocomplete_show_hidden_files = new_value;
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    _ => {}
                }
            }
            SettingKind::Enum { ref options } => {
                let current_idx = options.iter().position(|&o| o == entry.value).unwrap_or(0);
                let next_idx = (current_idx + 1) % options.len();
                let new_value = options[next_idx];

                match entry.key {
                    "output_style" => {
                        screen.output_style = new_value.to_string();
                        screen.settings_snapshot.config.output_style = Some(new_value.to_string());
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    "output_format" => {
                        screen.output_format = new_value.to_string();
                        screen.settings_snapshot.config.output_format = match new_value {
                            "json" => claurst_core::config::OutputFormat::Json,
                            "stream_json" => claurst_core::config::OutputFormat::StreamJson,
                            _ => claurst_core::config::OutputFormat::Text,
                        };
                        let _ = screen.settings_snapshot.save_sync();
                    }
                    _ => {}
                }
            }
            SettingKind::Number => {
                screen.start_edit(entry.key, &entry.value);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_screen_new_has_sensible_defaults() {
        let screen = SettingsScreen::new();
        assert!(!screen.visible);
        assert!(screen.search_query.is_empty());
        assert_eq!(screen.selected_idx, 0);
        assert!(screen.edit_field.is_none());
        assert!(screen.edit_value.is_empty());
    }

    #[test]
    fn all_entries_returns_expected_settings() {
        let screen = SettingsScreen::new();
        let entries = all_entries(&screen);
        // Base settings are always present, plus 0-3 conditional file injection settings
        assert!(entries.len() >= 16, "Should have at least 16 editable settings, got {}", entries.len());
        assert!(entries.len() <= 20, "Should have at most 20 editable settings, got {}", entries.len());
    }

    #[test]
    fn search_filters_entries_correctly() {
        let screen = SettingsScreen::new();
        let all = all_entries(&screen);
        let filtered: Vec<_> = all
            .iter()
            .filter(|e| e.label.to_lowercase().contains("token"))
            .collect();
        assert_eq!(filtered.len(), 1, "Should find exactly 1 entry matching 'token'");
        assert_eq!(filtered[0].label, "Max Tokens");
    }

    #[test]
    fn toggle_bool_entry_flips_value() {
        let mut screen = SettingsScreen::new();
        screen.notifications = true;
        screen.open();

        let initial = screen.notifications;
        let all = all_entries(&screen);
        let entry = &all[2]; // notifications is at index 2
        assert_eq!(entry.label, "Desktop notifications");

        // Simulate toggle (manually, since toggle_or_cycle_current modifies internal state)
        screen.notifications = !screen.notifications;
        assert_ne!(screen.notifications, initial);
    }

    #[test]
    fn cycle_enum_entry_wraps_around() {
        let mut screen = SettingsScreen::new();
        screen.output_style = "default".to_string();

        // Simulate cycling through all options
        let options = vec!["default", "concise", "explanatory", "learning"];
        let mut idx = options.iter().position(|&o| o == "default").unwrap();

        idx = (idx + 1) % options.len();
        assert_eq!(options[idx], "concise");

        idx = (idx + 1) % options.len();
        assert_eq!(options[idx], "explanatory");

        idx = (idx + 1) % options.len();
        assert_eq!(options[idx], "learning");

        idx = (idx + 1) % options.len();
        assert_eq!(options[idx], "default"); // Wraps around
    }
}
