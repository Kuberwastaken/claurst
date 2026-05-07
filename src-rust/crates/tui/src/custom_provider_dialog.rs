// custom_provider_dialog.rs — Modal dialog for entering a custom provider URL and API key.
//
// Collects both a base URL and an API key for the custom OpenAI-compatible
// provider used by /connect.

use ratatui::layout::Rect;
use ratatui::prelude::Stylize;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::overlays::{centered_rect, render_dark_overlay, render_dialog_bg, CLAURST_PANEL_BG};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CustomProviderField {
    Url,
    ApiKey,
}

pub struct CustomProviderDialogState {
    pub visible: bool,
    pub provider_id: String,
    pub provider_name: String,
    pub url_input: String,
    pub api_key_input: String,
    pub active_field: CustomProviderField,
}

impl CustomProviderDialogState {
    pub fn new() -> Self {
        Self {
            visible: false,
            provider_id: String::new(),
            provider_name: String::new(),
            url_input: String::new(),
            api_key_input: String::new(),
            active_field: CustomProviderField::Url,
        }
    }

    pub fn open(&mut self, provider_id: String, provider_name: String, current_url: Option<String>) {
        self.visible = true;
        self.provider_id = provider_id;
        self.provider_name = provider_name;
        self.url_input = current_url.unwrap_or_default();
        self.api_key_input.clear();
        self.active_field = CustomProviderField::Url;
    }

    pub fn close(&mut self) {
        self.visible = false;
        self.url_input.clear();
        self.api_key_input.clear();
        self.active_field = CustomProviderField::Url;
    }

    pub fn move_next_field(&mut self) {
        self.active_field = match self.active_field {
            CustomProviderField::Url => CustomProviderField::ApiKey,
            CustomProviderField::ApiKey => CustomProviderField::Url,
        };
    }

    pub fn move_prev_field(&mut self) {
                self.active_field = match self.active_field {
            CustomProviderField::Url => CustomProviderField::ApiKey,
            CustomProviderField::ApiKey => CustomProviderField::Url,
        };
    }

    pub fn insert_char(&mut self, c: char) {
        match self.active_field {
            CustomProviderField::Url => self.url_input.push(c),
            CustomProviderField::ApiKey => self.api_key_input.push(c),
        }
    }

    pub fn backspace(&mut self) {
        match self.active_field {
            CustomProviderField::Url => {
                self.url_input.pop();
            }
            CustomProviderField::ApiKey => {
                self.api_key_input.pop();
            }
        }
    }

    pub fn can_submit(&self) -> bool {
        !self.url_input.trim().is_empty()
    }

    pub fn take_values(&mut self) -> (String, String) {
        let url = self.url_input.trim().to_string();
        let api_key = self.api_key_input.clone();
        self.close();
        (url, api_key)
    }
}

pub fn render_custom_provider_dialog(
    frame: &mut Frame,
    state: &CustomProviderDialogState,
    area: Rect,
) {
    if !state.visible {
        return;
    }

    let pink = Color::Rgb(233, 30, 99);
    let dim = Color::Rgb(90, 90, 90);
    let muted = Color::Rgb(180, 180, 180);
    let dialog_bg = CLAURST_PANEL_BG;

    render_dark_overlay(frame, area);

    let width = 76u16.min(area.width.saturating_sub(4));
    let height = 13u16;
    let dialog_area = centered_rect(width, height, area);
    render_dialog_bg(frame, dialog_area);

    let inner = Rect {
        x: dialog_area.x + 1,
        y: dialog_area.y + 1,
        width: dialog_area.width.saturating_sub(2),
        height: dialog_area.height.saturating_sub(2),
    };

    let title_text = format!("Connect {}", state.provider_name);
    let title_pad = inner.width.saturating_sub(title_text.len() as u16 + 5) as usize;

    let url_style = if state.active_field == CustomProviderField::Url {
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let key_style = if state.active_field == CustomProviderField::ApiKey {
        Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };

    let url_text = if state.url_input.is_empty() {
        "https://your-openai-compatible-endpoint/v1".to_string()
    } else {
        state.url_input.clone()
    };

    let masked_key = if state.api_key_input.is_empty() {
        "paste your API key here...".to_string()
    } else {
        let chars: Vec<char> = state.api_key_input.chars().collect();
        if chars.len() <= 4 {
            state.api_key_input.clone()
        } else {
            let visible: String = chars[chars.len() - 4..].iter().collect();
            format!("{}{}", "•".repeat(chars.len() - 4), visible)
        }
    };

    let confirm_hint = if state.can_submit() {
        " enter confirm"
    } else {
        " fill URL field"
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
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
    lines.push(Line::from(vec![Span::styled(" URL:", Style::default().fg(muted))]));
    lines.push(Line::from(vec![
        Span::styled(format!(" {}", url_text), url_style),
        Span::styled(
            if state.active_field == CustomProviderField::Url { "_" } else { "" },
            Style::default().fg(pink),
        ),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(" API Key:", Style::default().fg(muted))]));
    lines.push(Line::from(vec![
        Span::styled(format!(" {}", masked_key), key_style),
        Span::styled(
            if state.active_field == CustomProviderField::ApiKey { "_" } else { "" },
            Style::default().fg(pink),
        ),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(" tab", Style::default().fg(dim)),
        Span::styled(" switch field  ", Style::default().fg(dim)),
        Span::styled(confirm_hint, Style::default().fg(dim)),
    ]));

    let para = Paragraph::new(lines).bg(dialog_bg);
    frame.render_widget(para, inner);
}
