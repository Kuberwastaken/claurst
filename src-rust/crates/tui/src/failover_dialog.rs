// failover_dialog.rs: Interactive provider failover chooser (port of
// jcode's provider-failover flow, adapted to claurst).
//
// When a model request fails with a failover-class error (auth, quota,
// 429/5xx after retries), the query loop attaches a structured
// `[claurst-provider-failover]` prompt to the error message. The TUI
// parses it in `handle_query_event`, opens this chooser, and the user
// switches provider+model with one keypress or dismisses with Esc.
//
// Reuses the DialogSelect widget (same fuzzy picker as /connect) so no
// new UI paradigm is introduced. Digit keys 1-9 select directly; 'a'
// marks the failed provider unavailable for the session.

use claurst_api::failover::ProviderFailoverPrompt;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::dialog_select::{DialogSelectState, SelectItem, render_dialog_select};
use crate::overlays::{CLAURST_PANEL_BG, centered_rect, render_dark_overlay};

/// Human label for a provider id, matching the /connect picker.
pub fn provider_label(provider_id: &str) -> &str {
    match provider_id {
        "anthropic" => "Anthropic",
        "openai" => "OpenAI",
        "openai-codex" | "codex" => "OpenAI Codex",
        "google" => "Google",
        "github-copilot" => "GitHub Copilot",
        "free" => "Free Mode",
        "cohere" => "Cohere",
        "minimax" => "MiniMax",
        "ollama" => "Ollama",
        "lmstudio" | "lm-studio" => "LM Studio",
        "llamacpp" | "llama-cpp" | "llama-server" => "llama.cpp",
        other => other,
    }
}

/// Session-scoped failover chooser state.
///
/// `unavailable` is the in-memory per-session set of providers the user
/// marked unavailable; it lives here (not in settings) so it clears on
/// restart.
#[derive(Debug, Default)]
pub struct FailoverDialogState {
    /// Currently visible chooser, built from the last failover prompt.
    pub picker: Option<DialogSelectState>,
    /// Provider the current chooser failed over from.
    pub from_provider: Option<String>,
    /// Whether choosing a route also marks the failed provider
    /// unavailable for the session.
    pub mark_unavailable: bool,
    /// Providers marked unavailable this session (in-memory only).
    pub unavailable: Vec<String>,
}

impl FailoverDialogState {
    /// Build a chooser from a parsed failover prompt. Returns `None` when
    /// the prompt carries no routes.
    pub fn open_for(&mut self, prompt: &ProviderFailoverPrompt) -> Option<()> {
        let items = prompt
            .routes
            .iter()
            .filter_map(|route| route_item(route))
            .collect::<Vec<_>>();
        if items.is_empty() {
            return None;
        }
        let mut picker = DialogSelectState::new("Switch provider", items);
        picker.open();
        self.picker = Some(picker);
        self.from_provider = Some(prompt.from_provider.clone());
        self.mark_unavailable = prompt.mark_unavailable;
        Some(())
    }

    pub fn close(&mut self) {
        self.picker = None;
        self.from_provider = None;
        self.mark_unavailable = false;
    }

    pub fn is_open(&self) -> bool {
        self.picker.as_ref().is_some_and(|p| p.visible)
    }

    pub fn mark_provider_unavailable(&mut self, provider_id: &str) {
        if !provider_id.is_empty() && !self.unavailable.contains(&provider_id.to_string()) {
            self.unavailable.push(provider_id.to_string());
        }
    }
}

/// Turn a failover route (`"provider/model"` or bare provider id) into a
/// picker item. Returns `None` for empty routes.
fn route_item(route: &str) -> Option<SelectItem> {
    let (provider_id, model) = match route.split_once('/') {
        Some((p, m)) => (p, m),
        None => (route, ""),
    };
    let label = provider_label(provider_id).to_string();
    let description = if model.is_empty() {
        "default model".to_string()
    } else {
        model.to_string()
    };
    Some(SelectItem {
        id: route.to_string(),
        title: label,
        description,
        category: "Failover".to_string(),
        badge: Some("SWITCH".to_string()),
    })
}

/// Render the failover chooser with a header explaining what failed.
pub fn render_failover_dialog(frame: &mut Frame, state: &FailoverDialogState, area: Rect) {
    let Some(picker) = state.picker.as_ref() else {
        return;
    };
    if !picker.visible {
        return;
    }
    render_dark_overlay(frame, area);

    // Header strip above the picker: what failed and why.
    let header_area = centered_rect(70, 6, area);
    let header = match &state.from_provider {
        Some(from) => {
            let label = provider_label(from);
            let note = if state.mark_unavailable {
                format!(
                    "{label} failed. Pick a replacement (1-9 or Enter). \
                     'a' also skips {label} for the rest of the session. Esc dismisses."
                )
            } else {
                format!(
                    "{label} failed this request. Pick a replacement \
                     (1-9 or Enter), or Esc to stay."
                )
            };
            Line::from(vec![
                Span::styled(
                    "Provider failover  ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(note, Style::default().fg(Color::Gray)),
            ])
        }
        None => Line::from(Span::styled(
            "Provider failover: pick a replacement (1-9 or Enter), Esc dismisses.",
            Style::default().fg(Color::Gray),
        )),
    };
    let block = ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .style(Style::default().bg(CLAURST_PANEL_BG));
    frame.render_widget(
        Paragraph::new(header)
            .wrap(ratatui::widgets::Wrap { trim: true })
            .block(block),
        header_area,
    );

    render_dialog_select(frame, picker, area);
}
