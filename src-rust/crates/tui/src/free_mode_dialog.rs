// free_mode_dialog.rs — Setup dialog for the composite "Free" provider.
//
// Walks the user through the multi-provider free-mode caveats and collects
// API keys from any subset of the supported upstreams. The chain stacks
// many free tiers (Groq, Cerebras, Google, Mistral, SambaNova, NVIDIA,
// Cohere, OpenRouter, OpenCode Zen, Z.AI, Zhipu) behind one synthetic
// `free/auto` model — the more keys the user pastes in, the more
// providers the router can fall back to. Minimum 1 key to enable; more
// is better.
//
// Layout:
//   ┌─ Connect Free (multi-provider) ───────────────── esc ┐
//   │  Stack the free tiers from many providers behind     │
//   │  one endpoint. ⚠ context management is worse than    │
//   │  paid models; long sessions truncate aggressively.   │
//   │                                                      │
//   │  Paste any keys you have — more = better availability│
//   │  and higher daily caps. Minimum 1 key to enable.     │
//   │                                                      │
//   │  ▸ Groq                          console.groq.com/.. │
//   │    ••••••••AbCd_                                     │
//   │    Cerebras                      cloud.cerebras.ai   │
//   │    paste your API key here...                        │
//   │    Google Gemini                 aistudio.google.com │
//   │    ••••••••wxyz                                      │
//   │    …7 more — tab/↑↓ to scroll                        │
//   │                                                      │
//   │  ↑/↓ next   enter confirm (1+ keys)                  │
//   └──────────────────────────────────────────────────────┘

use ratatui::layout::Rect;
use ratatui::prelude::Stylize;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use claurst_api::{FreeUpstream, FREE_CATALOG};

use crate::overlays::{centered_rect, render_dark_overlay, render_dialog_bg, CLAURST_PANEL_BG};

/// One row in the dialog — one provider's name, URL, and the user's
/// (possibly empty) typed key.
#[derive(Debug, Clone)]
pub struct FreeModeField {
    pub upstream: &'static FreeUpstream,
    pub key: String,
}

pub struct FreeModeDialogState {
    pub visible: bool,
    pub fields: Vec<FreeModeField>,
    pub active_idx: usize,
    /// First visible field index (for scrolling when fields > viewport).
    pub scroll_offset: usize,
}

impl Default for FreeModeDialogState {
    fn default() -> Self {
        Self::new()
    }
}

impl FreeModeDialogState {
    pub fn new() -> Self {
        let fields = FREE_CATALOG
            .iter()
            .map(|upstream| FreeModeField {
                upstream,
                key: String::new(),
            })
            .collect();
        Self {
            visible: false,
            fields,
            active_idx: 0,
            scroll_offset: 0,
        }
    }

    /// Open the dialog, pre-populating each row from `existing[upstream.id]`
    /// when present.
    pub fn open(&mut self, existing: &[(&str, String)]) {
        self.visible = true;
        for field in &mut self.fields {
            field.key.clear();
        }
        for (id, key) in existing {
            if let Some(field) = self.fields.iter_mut().find(|f| f.upstream.id == *id) {
                field.key = key.clone();
            }
        }
        // Start on the first empty field, or the first field if none are empty.
        self.active_idx = self
            .fields
            .iter()
            .position(|f| f.key.is_empty())
            .unwrap_or(0);
        self.scroll_offset = 0;
        self.ensure_active_visible();
    }

    pub fn close(&mut self) {
        self.visible = false;
        for field in &mut self.fields {
            field.key.clear();
        }
        self.active_idx = 0;
        self.scroll_offset = 0;
    }

    /// Number of rows shown at once in the scrolling viewport.
    pub const VISIBLE_ROWS: usize = 4;

    pub fn move_next(&mut self) {
        if self.fields.is_empty() {
            return;
        }
        self.active_idx = (self.active_idx + 1) % self.fields.len();
        self.ensure_active_visible();
    }

    pub fn move_prev(&mut self) {
        if self.fields.is_empty() {
            return;
        }
        self.active_idx = if self.active_idx == 0 {
            self.fields.len() - 1
        } else {
            self.active_idx - 1
        };
        self.ensure_active_visible();
    }

    fn ensure_active_visible(&mut self) {
        if self.active_idx < self.scroll_offset {
            self.scroll_offset = self.active_idx;
        } else if self.active_idx >= self.scroll_offset + Self::VISIBLE_ROWS {
            self.scroll_offset = self.active_idx + 1 - Self::VISIBLE_ROWS;
        }
    }

    pub fn insert_char(&mut self, c: char) {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            field.key.push(c);
        }
    }

    pub fn backspace(&mut self) {
        if let Some(field) = self.fields.get_mut(self.active_idx) {
            field.key.pop();
        }
    }

    /// Enabling Free mode requires at least one non-empty key. More is better.
    pub fn can_submit(&self) -> bool {
        self.fields.iter().any(|f| !f.key.trim().is_empty())
    }

    pub fn filled_count(&self) -> usize {
        self.fields
            .iter()
            .filter(|f| !f.key.trim().is_empty())
            .count()
    }

    /// Consume the dialog state, returning every non-empty `(provider_id, key)`
    /// pair the user entered.
    pub fn take_values(&mut self) -> Vec<(&'static str, String)> {
        let out: Vec<(&'static str, String)> = self
            .fields
            .iter()
            .filter_map(|f| {
                let trimmed = f.key.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some((f.upstream.id, trimmed.to_string()))
                }
            })
            .collect();
        self.close();
        out
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn mask_key(input: &str) -> String {
    if input.is_empty() {
        "paste your API key here...".to_string()
    } else {
        let chars: Vec<char> = input.chars().collect();
        if chars.len() <= 4 {
            input.to_string()
        } else {
            let tail: String = chars[chars.len() - 4..].iter().collect();
            format!("{}{}", "\u{2022}".repeat(chars.len() - 4), tail)
        }
    }
}

pub fn render_free_mode_dialog(frame: &mut Frame, state: &FreeModeDialogState, area: Rect) {
    if !state.visible {
        return;
    }

    let pink = Color::Rgb(233, 30, 99);
    let dim = Color::Rgb(90, 90, 90);
    let muted = Color::Rgb(180, 180, 180);
    let tip = Color::Rgb(120, 210, 150);
    let dialog_bg = CLAURST_PANEL_BG;

    render_dark_overlay(frame, area);

    let width = 84u16.min(area.width.saturating_sub(4));
    let height = 24u16.min(area.height.saturating_sub(2));
    let dialog_area = centered_rect(width, height, area);
    render_dialog_bg(frame, dialog_area);

    let inner = Rect {
        x: dialog_area.x + 1,
        y: dialog_area.y + 1,
        width: dialog_area.width.saturating_sub(2),
        height: dialog_area.height.saturating_sub(2),
    };

    let total = state.fields.len();
    let filled = state.filled_count();
    let title_text = format!(
        "Connect Free (multi-provider \u{2014} {}/{} keys)",
        filled, total
    );
    let title_pad = inner
        .width
        .saturating_sub(title_text.chars().count() as u16 + 5) as usize;

    let confirm_hint = if state.can_submit() {
        format!(
            " enter confirm ({} key{} — more = better)",
            filled,
            if filled == 1 { "" } else { "s" }
        )
    } else {
        " paste at least 1 key — as many as you can add is better".to_string()
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    // Title row
    lines.push(Line::from(vec![
        Span::styled(
            format!(" {}", title_text),
            Style::default().fg(pink).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{:>width$}", "esc ", width = title_pad),
            Style::default().fg(dim),
        ),
    ]));
    lines.push(Line::from(""));

    // Description (one tight line) + tip.
    lines.push(Line::from(vec![Span::styled(
        " Stack free tiers behind one endpoint.",
        Style::default().fg(muted),
    )]));
    lines.push(Line::from(vec![
        Span::styled(
            " TIP ",
            Style::default().fg(tip).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            "More keys = better availability and higher caps.",
            Style::default().fg(tip),
        ),
    ]));
    lines.push(Line::from(""));

    // Field viewport
    let start = state.scroll_offset;
    let end = (start + FreeModeDialogState::VISIBLE_ROWS).min(state.fields.len());
    if start > 0 {
        lines.push(Line::from(vec![Span::styled(
            format!("   \u{2191} {} above", start),
            Style::default().fg(dim),
        )]));
    }

    let row_label_width: usize = state
        .fields
        .iter()
        .map(|f| f.upstream.title.chars().count())
        .max()
        .unwrap_or(0)
        .max(8);

    for idx in start..end {
        let field = &state.fields[idx];
        let active = idx == state.active_idx;
        let marker = if active { "\u{25b8}" } else { " " };
        let label_style = if active {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(muted)
        };
        let url_style = Style::default().fg(dim);

        let label_padded = format!("{:<width$}", field.upstream.title, width = row_label_width);
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", marker), Style::default().fg(pink)),
            Span::styled(label_padded, label_style),
            Span::styled("   ", Style::default()),
            Span::styled(field.upstream.key_url.to_string(), url_style),
        ]));

        let masked = mask_key(&field.key);
        let input_style = if field.key.is_empty() {
            Style::default().fg(dim)
        } else if active {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let cursor = if active { "_" } else { "" };
        lines.push(Line::from(vec![
            Span::styled("     ", Style::default()),
            Span::styled(masked, input_style),
            Span::styled(cursor.to_string(), Style::default().fg(pink)),
        ]));
    }

    if end < state.fields.len() {
        lines.push(Line::from(vec![Span::styled(
            format!("   \u{2193} {} more", state.fields.len() - end),
            Style::default().fg(dim),
        )]));
    }

    lines.push(Line::from(""));

    // Footer
    lines.push(Line::from(vec![
        Span::styled(" \u{2191}/\u{2193}", Style::default().fg(dim)),
        Span::styled(" next field   ", Style::default().fg(dim)),
        Span::styled(confirm_hint, Style::default().fg(dim)),
    ]));

    let para = Paragraph::new(lines).bg(dialog_bg);
    frame.render_widget(para, inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_hidden() {
        let s = FreeModeDialogState::new();
        assert!(!s.visible);
        assert_eq!(s.fields.len(), FREE_CATALOG.len());
    }

    #[test]
    fn open_starts_on_first_empty_field() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        assert!(s.visible);
        assert_eq!(s.active_idx, 0);
    }

    #[test]
    fn open_seeds_existing_keys_and_skips_to_first_empty() {
        let mut s = FreeModeDialogState::new();
        s.open(&[(FREE_CATALOG[0].id, "existing-key".to_string())]);
        assert_eq!(s.fields[0].key, "existing-key");
        // First empty is the second field.
        assert_eq!(s.active_idx, 1);
    }

    #[test]
    fn move_next_wraps() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        let n = s.fields.len();
        s.active_idx = n - 1;
        s.move_next();
        assert_eq!(s.active_idx, 0);
    }

    #[test]
    fn move_prev_wraps() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        s.active_idx = 0;
        s.move_prev();
        assert_eq!(s.active_idx, s.fields.len() - 1);
    }

    #[test]
    fn scroll_offset_follows_active() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        for _ in 0..FreeModeDialogState::VISIBLE_ROWS {
            s.move_next();
        }
        assert!(s.scroll_offset > 0);
        assert!(s.active_idx >= s.scroll_offset);
        assert!(s.active_idx < s.scroll_offset + FreeModeDialogState::VISIBLE_ROWS);
    }

    #[test]
    fn insert_and_backspace_target_active_field() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        s.insert_char('a');
        s.insert_char('b');
        assert_eq!(s.fields[0].key, "ab");
        s.backspace();
        assert_eq!(s.fields[0].key, "a");
    }

    #[test]
    fn can_submit_requires_at_least_one_key() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        assert!(!s.can_submit());
        s.insert_char('k');
        assert!(s.can_submit());
    }

    #[test]
    fn take_values_returns_only_non_empty_trimmed_pairs_and_closes() {
        let mut s = FreeModeDialogState::new();
        s.open(&[]);
        s.insert_char(' ');
        s.insert_char('a');
        s.insert_char(' ');
        s.move_next();
        s.insert_char('b');
        let values = s.take_values();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0], (FREE_CATALOG[0].id, "a".to_string()));
        assert_eq!(values[1], (FREE_CATALOG[1].id, "b".to_string()));
        assert!(!s.visible);
    }

    #[test]
    fn mask_key_hides_all_but_last_four() {
        assert_eq!(mask_key(""), "paste your API key here...");
        assert_eq!(mask_key("abc"), "abc");
        assert_eq!(mask_key("abcdefgh"), "\u{2022}\u{2022}\u{2022}\u{2022}efgh");
    }
}
