//! Read-only viewer for `[Pasted text #N ...]` placeholders.
//!
//! Clicking a placeholder in the prompt opens this scrollable modal so the
//! full pasted body can be read without splicing hundreds of lines into the
//! prompt buffer. Alt+E (from the prompt or from inside the viewer) still
//! performs the in-place expansion for editing.

use std::cell::Cell;

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use unicode_width::UnicodeWidthChar;

use crate::overlays::{
    begin_modal_buf, render_modal_title_buf, CLAURST_MUTED, CLAURST_TEXT,
};

/// Rows scrolled per PageUp/PageDown press.
const PAGE_ROWS: usize = 10;

/// Modal state for viewing a stored paste body without expanding it into the
/// prompt buffer.
#[derive(Debug, Clone, Default)]
pub struct PasteViewer {
    pub visible: bool,
    /// ID of the `[Pasted text #N ...]` placeholder being viewed.
    pub paste_id: u32,
    /// The paste body split into logical lines (tabs pre-expanded).
    lines: Vec<String>,
    /// Scroll offset in wrapped visual rows.
    pub scroll: usize,
    /// Max meaningful scroll measured by the last render (wrapped rows minus
    /// viewport rows). Scrolling is clamped to it, mirroring the transcript's
    /// `last_max_scroll` pattern.
    last_max_scroll: Cell<usize>,
}

impl PasteViewer {
    pub fn open(&mut self, paste_id: u32, body: &str) {
        self.paste_id = paste_id;
        // Tabs render with unpredictable widths in a terminal cell grid;
        // expand them so wrapping arithmetic stays exact.
        self.lines = body
            .lines()
            .map(|l| l.replace('\t', "    "))
            .collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.scroll = 0;
        self.last_max_scroll.set(0);
        self.visible = true;
    }

    pub fn close(&mut self) {
        self.visible = false;
        self.lines.clear();
        self.scroll = 0;
    }

    /// Number of logical (unwrapped) lines in the paste body.
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn scroll_up(&mut self, rows: usize) {
        self.scroll = self.scroll.saturating_sub(rows);
    }

    pub fn scroll_down(&mut self, rows: usize) {
        self.scroll = (self.scroll + rows).min(self.last_max_scroll.get());
    }

    pub fn page_up(&mut self) {
        self.scroll_up(PAGE_ROWS);
    }

    pub fn page_down(&mut self) {
        self.scroll_down(PAGE_ROWS);
    }

    pub fn scroll_to_top(&mut self) {
        self.scroll = 0;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll = self.last_max_scroll.get();
    }

    /// Hard-wrap every logical line to `width` columns (unicode-width aware)
    /// so scroll units map 1:1 onto rendered rows.
    fn wrapped_rows(&self, width: usize) -> Vec<String> {
        let mut rows = Vec::with_capacity(self.lines.len());
        for line in &self.lines {
            let mut row = String::new();
            let mut used = 0usize;
            for c in line.chars() {
                let w = c.width().unwrap_or(0);
                if used + w > width && !row.is_empty() {
                    rows.push(std::mem::take(&mut row));
                    used = 0;
                }
                row.push(c);
                used += w;
            }
            rows.push(row);
        }
        rows
    }
}

/// Render the paste viewer modal into `buf`, clamping the scroll offset to
/// the measured content height as a side effect.
pub fn render_paste_viewer_buf(viewer: &PasteViewer, area: Rect, buf: &mut Buffer) {
    let width = area.width.saturating_sub(6).clamp(20, 100);
    let height = area.height.saturating_sub(4).clamp(8, 40);
    let layout = begin_modal_buf(buf, area, width, height, 2, 1);

    render_modal_title_buf(
        buf,
        layout.header_area,
        &format!("Pasted text #{}", viewer.paste_id),
        &format!("{} lines", viewer.line_count()),
    );

    let body = layout.body_area;
    if body.width == 0 || body.height == 0 {
        return;
    }
    let text_width = body.width.saturating_sub(2) as usize;
    if text_width == 0 {
        return;
    }
    let rows = viewer.wrapped_rows(text_width);
    let max_scroll = rows.len().saturating_sub(body.height as usize);
    viewer.last_max_scroll.set(max_scroll);
    let scroll = viewer.scroll.min(max_scroll);

    for (i, row) in rows
        .iter()
        .skip(scroll)
        .take(body.height as usize)
        .enumerate()
    {
        let line = Line::from(Span::styled(
            row.clone(),
            Style::default().fg(CLAURST_TEXT),
        ));
        Paragraph::new(line).render(
            Rect {
                x: body.x + 1,
                y: body.y + i as u16,
                width: body.width.saturating_sub(2),
                height: 1,
            },
            buf,
        );
    }

    let footer = Line::from(Span::styled(
        " ↑/↓ scroll · PgUp/PgDn page · g/G top/bottom · Alt+E insert into prompt · Esc close",
        Style::default().fg(CLAURST_MUTED),
    ));
    Paragraph::new(footer).render(layout.footer_area, buf);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_splits_lines_and_resets_scroll() {
        let mut v = PasteViewer {
            scroll: 7,
            ..Default::default()
        };
        v.open(3, "a\nb\tc\nd");
        assert!(v.visible);
        assert_eq!(v.paste_id, 3);
        assert_eq!(v.line_count(), 3);
        assert_eq!(v.scroll, 0);
        // Tab expanded so wrap math stays width-exact.
        assert_eq!(v.lines[1], "b    c");
    }

    #[test]
    fn scroll_clamps_to_last_render_measurement() {
        let mut v = PasteViewer::default();
        v.open(1, "x\ny\nz");
        v.last_max_scroll.set(5);
        v.scroll_down(100);
        assert_eq!(v.scroll, 5);
        v.scroll_up(2);
        assert_eq!(v.scroll, 3);
        v.scroll_to_bottom();
        assert_eq!(v.scroll, 5);
        v.scroll_to_top();
        assert_eq!(v.scroll, 0);
    }

    #[test]
    fn wrapped_rows_hard_wraps_wide_lines() {
        let mut v = PasteViewer::default();
        v.open(1, "abcdefghij\nshort");
        let rows = v.wrapped_rows(4);
        assert_eq!(rows, vec!["abcd", "efgh", "ij", "shor", "t"]);
    }

    #[test]
    fn close_drops_body() {
        let mut v = PasteViewer::default();
        v.open(1, "big body");
        v.close();
        assert!(!v.visible);
        assert_eq!(v.line_count(), 0);
    }
}
