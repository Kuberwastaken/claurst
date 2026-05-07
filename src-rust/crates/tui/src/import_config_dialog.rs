use claurst_core::import_config::{ImportPreview, ImportSelection, PreviewAction};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Frame;

use crate::overlays::{
    begin_modal_frame, modal_header_line_area, render_modal_title_frame, CLAURST_ACCENT,
    CLAURST_MUTED, CLAURST_PANEL_BG, CLAURST_TEXT,
};

#[derive(Debug, Clone, Default)]
pub struct ImportConfigDialogState {
    pub visible: bool,
    pub selection: Option<ImportSelection>,
    pub preview: Option<ImportPreview>,
}

impl ImportConfigDialogState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&mut self, preview: ImportPreview) {
        self.visible = true;
        self.selection = Some(preview.selection);
        self.preview = Some(preview);
    }

    pub fn close(&mut self) {
        self.visible = false;
        self.selection = None;
        self.preview = None;
    }
}

pub fn render_import_config_dialog(
    frame: &mut Frame,
    state: &ImportConfigDialogState,
    area: Rect,
) {
    if !state.visible {
        return;
    }

    let Some(preview) = &state.preview else {
        return;
    };

    let layout = begin_modal_frame(frame, area, 92, 28, 2, 1);
    render_modal_title_frame(frame, layout.header_area, "Import config", "esc");
    if let Some(subtitle_area) = modal_header_line_area(layout.header_area, 1) {
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                " Preview the content to import from ~/.claude; Enter to confirm, Esc to cancel.",
                Style::default().fg(CLAURST_MUTED),
            )])),
            subtitle_area,
        );
    }

    let mut lines: Vec<Line<'static>> = vec![];
    if let Some(doc) = &preview.claude_md {
        lines.push(section_title("CLAUDE.md"));
        lines.push(path_row("Source", &doc.plan.source_path.display().to_string()));
        lines.push(path_row("Target", &doc.plan.target_path.display().to_string()));
        lines.push(meta_row(&format!(
            "{} lines, {} chars, {}",
            doc.line_count,
            doc.char_count,
            if doc.plan.target_exists { "will overwrite the target file" } else { "will create the target file" }
        )));
        for line in doc.excerpt.lines() {
            lines.push(Line::from(vec![Span::styled(
                format!("  {}", line),
                Style::default().fg(CLAURST_TEXT),
            )]));
        }
        lines.push(Line::from(""));
    }

    if let Some(settings) = &preview.settings {
        lines.push(section_title("settings.json"));
        lines.push(path_row("Source", &settings.plan.source_path.display().to_string()));
        lines.push(path_row("Target", &settings.plan.target_path.display().to_string()));
        lines.push(meta_row(&format!(
            "Import {}, replace {}, keep {}, skip {} fields",
            settings.imported_count,
            settings.replaced_count,
            settings.kept_count,
            settings.skipped_count
        )));
        for field in &settings.fields {
            let action_style = match field.action {
                PreviewAction::Import => Style::default().fg(CLAURST_ACCENT).add_modifier(Modifier::BOLD),
                PreviewAction::Replace => Style::default().fg(CLAURST_ACCENT).add_modifier(Modifier::BOLD),
                PreviewAction::Keep => Style::default().fg(CLAURST_MUTED),
                PreviewAction::Skip => Style::default().fg(CLAURST_MUTED),
            };
            let mut spans = vec![
                Span::styled(format!("  [{}] ", field.action.label()), action_style),
                Span::styled(field.name.clone(), Style::default().fg(CLAURST_TEXT)),
            ];
            if let Some(reason) = &field.reason {
                spans.push(Span::styled(
                    format!(" — {}", reason),
                    Style::default().fg(CLAURST_MUTED),
                ));
            }
            lines.push(Line::from(spans));
        }
    }

    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .style(Style::default().bg(CLAURST_PANEL_BG)),
        layout.body_area,
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![Span::styled(
            " Enter to import  ·  Esc to cancel",
            Style::default().fg(CLAURST_MUTED).add_modifier(Modifier::ITALIC),
        )])),
        layout.footer_area,
    );
}

fn section_title(title: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(" ", Style::default()),
        Span::styled(title.to_string(), Style::default().fg(CLAURST_ACCENT).add_modifier(Modifier::BOLD)),
    ])
}

fn path_row(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("  {}: ", label), Style::default().fg(CLAURST_MUTED)),
        Span::styled(value.to_string(), Style::default().fg(CLAURST_TEXT)),
    ])
}

fn meta_row(text: &str) -> Line<'static> {
    Line::from(vec![Span::styled(
        format!("  {}", text),
        Style::default().fg(CLAURST_MUTED),
    )])
}
