//! VirtualMessageList — efficient scrollable list that renders only visible items.
//! Mirrors src/components/VirtualMessageList.tsx.
//!
//! Key idea: each item has a cached height (in terminal rows). We track a
//! `scroll_offset` (rows from top) and render only items whose row ranges
//! intersect the viewport.

use ratatui::{buffer::Buffer, layout::Rect};
use std::collections::HashMap;

/// Trait that list items must implement so the virtual list can measure
/// and render them.
pub trait VirtualItem {
    /// Estimate or compute the rendered height of this item at `width` columns.
    fn measure_height(&self, width: u16) -> u16;

    /// Render the item into `buf` at `area`.
    fn render(&self, area: Rect, buf: &mut Buffer, selected: bool);

    /// Return a searchable text representation of this item.
    fn search_text(&self) -> String;

    /// Returns true if this item is a section header that should be pinned
    /// at the top of the viewport when scrolled past.
    fn is_section_header(&self) -> bool {
        false
    }
}

/// Virtual scrolling list.
pub struct VirtualList<T: VirtualItem> {
    /// All items (messages, results, etc.).
    pub items: Vec<T>,

    /// Height cache: (item_index, terminal_width) → row_count.
    height_cache: HashMap<(usize, u16), u16>,

    /// Current scroll offset in rows from the top of all items.
    pub scroll_offset: u16,

    /// Terminal viewport height in rows.
    pub viewport_height: u16,

    /// If true, always scroll to the bottom when new items are added.
    pub sticky_bottom: bool,

    /// Index of the currently selected item (for keyboard navigation).
    pub selected_index: Option<usize>,

    /// Pre-built search index: item_index → searchable_text.
    search_index: Vec<String>,

    /// Last search query (cached for performance).
    last_search: Option<String>,
    /// Cached search match indices.
    search_matches: Vec<usize>,
}

impl<T: VirtualItem> VirtualList<T> {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            height_cache: HashMap::new(),
            scroll_offset: 0,
            viewport_height: 24,
            sticky_bottom: true,
            selected_index: None,
            search_index: Vec::new(),
            last_search: None,
            search_matches: Vec::new(),
        }
    }

    /// Replace all items and rebuild the search index.
    pub fn set_items(&mut self, items: Vec<T>) {
        self.search_index = items.iter().map(|i| i.search_text()).collect();
        self.items = items;
        self.height_cache.clear();
        if self.sticky_bottom {
            self.jump_to_bottom();
        }
        // Invalidate search cache
        self.last_search = None;
        self.search_matches.clear();
    }

    /// Push a single item and optionally scroll to bottom.
    pub fn push_item(&mut self, item: T) {
        self.search_index.push(item.search_text());
        self.items.push(item);
        if self.sticky_bottom {
            self.jump_to_bottom();
        }
    }

    /// Notify that the terminal has been resized; invalidate the height cache.
    pub fn on_resize(&mut self, new_viewport_height: u16) {
        self.viewport_height = new_viewport_height;
        self.height_cache.clear();
    }

    /// Get the cached height for item `idx` at `width`, computing it if needed.
    fn item_height(&mut self, idx: usize, width: u16) -> u16 {
        let key = (idx, width);
        if let Some(&h) = self.height_cache.get(&key) {
            return h;
        }
        let h = if idx < self.items.len() {
            self.items[idx].measure_height(width).max(1)
        } else {
            1
        };
        self.height_cache.insert(key, h);
        h
    }

    /// Total height of all items at `width`.
    pub fn total_height(&mut self, width: u16) -> u16 {
        (0..self.items.len())
            .map(|i| self.item_height(i, width))
            .sum::<u16>()
    }

    /// Scroll so item `idx` is visible, with 3 rows of headroom above.
    pub fn scroll_to_index(&mut self, idx: usize, width: u16) {
        let mut row = 0u16;
        for i in 0..idx.min(self.items.len()) {
            row = row.saturating_add(self.item_height(i, width));
        }
        // Put it 3 rows from the top of viewport
        self.scroll_offset = row.saturating_sub(3);
    }

    /// Scroll to the very bottom.
    pub fn jump_to_bottom(&mut self) {
        // We don't know viewport height in advance without width — set a high value;
        // render() will clamp scroll_offset appropriately.
        self.scroll_offset = u16::MAX;
    }

    /// Scroll up by `rows` rows.
    pub fn scroll_up(&mut self, rows: u16) {
        self.scroll_offset = self.scroll_offset.saturating_sub(rows);
        self.sticky_bottom = false;
    }

    /// Scroll down by `rows` rows.
    pub fn scroll_down(&mut self, rows: u16, width: u16) {
        let total = self.total_height(width);
        let max_offset = total.saturating_sub(self.viewport_height);
        self.scroll_offset = (self.scroll_offset + rows).min(max_offset);
        if self.scroll_offset >= max_offset {
            self.sticky_bottom = true;
        }
    }

    /// Find the index of the section header that should be pinned at the top.
    /// This is the last header item that lies entirely above `scroll_offset`.
    pub fn sticky_header_index(&mut self, width: u16) -> Option<usize> {
        let mut row = 0u16;
        let mut last_header: Option<usize> = None;
        for i in 0..self.items.len() {
            let h = self.item_height(i, width);
            if row + h > self.scroll_offset {
                // This item is in or after the viewport
                break;
            }
            if self.items[i].is_section_header() {
                last_header = Some(i);
            }
            row += h;
        }
        last_header
    }

    /// Render visible items into `buf` within `area`.
    pub fn render(&mut self, area: Rect, buf: &mut Buffer) {
        if self.items.is_empty() || area.height == 0 {
            return;
        }

        self.viewport_height = area.height;
        let width = area.width;

        // Clamp scroll_offset
        let total = self.total_height(width);
        let max_offset = total.saturating_sub(area.height);
        if self.scroll_offset > max_offset {
            self.scroll_offset = max_offset;
        }

        let mut current_row = 0u16; // absolute row position of current item
        let mut screen_row = area.y; // where to render on screen

        for idx in 0..self.items.len() {
            let h = self.item_height(idx, width);
            let item_end = current_row + h;

            // Skip items entirely above the viewport
            if item_end <= self.scroll_offset {
                current_row = item_end;
                continue;
            }

            // Stop if we're past the viewport
            if current_row >= self.scroll_offset + area.height {
                break;
            }

            // Compute the portion of this item that's visible
            let visible_start = self.scroll_offset.saturating_sub(current_row);
            let visible_rows = h
                .saturating_sub(visible_start)
                .min(area.y + area.height - screen_row);

            if visible_rows == 0 {
                current_row = item_end;
                continue;
            }

            let item_area = Rect {
                x: area.x,
                y: screen_row,
                width: area.width,
                height: visible_rows,
            };

            // Clear the row(s) this item occupies before painting it. Item
            // renderers use ratatui's `Paragraph`, which only *styles* (never
            // clears) the cells past the end of a line — so a shorter line that
            // scrolls into a row previously holding a longer one would let the
            // old trailing text ghost through. (The sticky-header path below
            // already does this; regular rows need it too.)
            for by in item_area.y..item_area.y.saturating_add(item_area.height) {
                for bx in item_area.x..item_area.x.saturating_add(item_area.width) {
                    if let Some(cell) = buf.cell_mut((bx, by)) {
                        cell.reset();
                    }
                }
            }

            let selected = self.selected_index == Some(idx);
            self.items[idx].render(item_area, buf, selected);

            screen_row += visible_rows;
            current_row = item_end;
        }

        // Overlay the sticky section header (if any) at the top of the viewport.
        // This ensures the user always knows which section they're in.
        if let Some(header_idx) = self.sticky_header_index(width) {
            // Only render the sticky header if it's not already visible at the top
            // (i.e., the item's virtual row is before scroll_offset)
            let mut row = 0u16;
            for i in 0..header_idx {
                row = row.saturating_add(self.item_height(i, width));
            }
            // row is now the virtual start of header_idx
            if row < self.scroll_offset {
                let h = self.item_height(header_idx, width).min(area.height);
                let header_area = Rect {
                    x: area.x,
                    y: area.y,
                    width: area.width,
                    height: h,
                };
                // Clear the background for the sticky header
                for by in header_area.y..header_area.y + h {
                    for bx in header_area.x..header_area.x + header_area.width {
                        if let Some(cell) = buf.cell_mut((bx, by)) {
                            cell.set_char(' ');
                        }
                    }
                }
                self.items[header_idx].render(header_area, buf, false);
            }
        }
    }

    /// Build/rebuild the search index (idempotent).
    pub fn warm_search_index(&mut self) {
        self.search_index = self.items.iter().map(|i| i.search_text()).collect();
    }

    /// Find indices of items matching `query` (case-insensitive substring).
    pub fn find_matches(&mut self, query: &str) -> &[usize] {
        if self.last_search.as_deref() == Some(query) {
            return &self.search_matches;
        }
        let q = query.to_lowercase();
        self.search_matches = self
            .search_index
            .iter()
            .enumerate()
            .filter(|(_, text)| text.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();
        self.last_search = Some(query.to_string());
        &self.search_matches
    }

    /// Scroll to the next search match after `current_idx`.
    pub fn next_match(&mut self, query: &str, current_idx: usize, width: u16) -> Option<usize> {
        let matches = self.find_matches(query).to_vec();
        let next = matches.iter().find(|&&i| i > current_idx).copied()
            .or_else(|| matches.first().copied());
        if let Some(idx) = next {
            self.scroll_to_index(idx, width);
        }
        next
    }

    /// Scroll to the previous search match before `current_idx`.
    pub fn prev_match(&mut self, query: &str, current_idx: usize, width: u16) -> Option<usize> {
        let matches = self.find_matches(query).to_vec();
        let prev = matches.iter().rev().find(|&&i| i < current_idx).copied()
            .or_else(|| matches.last().copied());
        if let Some(idx) = prev {
            self.scroll_to_index(idx, width);
        }
        prev
    }
}

impl<T: VirtualItem> Default for VirtualList<T> {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Line;
    use ratatui::widgets::{Paragraph, Widget};

    /// Minimal item that renders one line via `Paragraph` — the same primitive
    /// the transcript uses, so it reproduces the trailing-cell ghosting.
    struct TextItem(String);
    impl VirtualItem for TextItem {
        fn measure_height(&self, _w: u16) -> u16 { 1 }
        fn render(&self, area: Rect, buf: &mut Buffer, _sel: bool) {
            Paragraph::new(Line::from(self.0.clone())).render(area, buf);
        }
        fn search_text(&self) -> String { self.0.clone() }
    }

    fn row_string(buf: &Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()).unwrap_or_else(|| " ".into()))
            .collect()
    }

    #[test]
    fn shorter_row_does_not_ghost_previous_longer_row() {
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);

        let mut list: VirtualList<TextItem> = VirtualList::new();
        list.sticky_bottom = false;
        list.set_items(vec![TextItem("LONGLINECONTENT1234".to_string())]);
        list.scroll_offset = 0;
        list.render(area, &mut buf);
        assert!(row_string(&buf, 0, 20).contains("LONGLINECONTENT"));

        // Render a shorter line into the SAME buffer (no reset) — the exact
        // situation a short line scrolling into a previously-long row creates.
        list.set_items(vec![TextItem("hi".to_string())]);
        list.scroll_offset = 0;
        list.render(area, &mut buf);

        let row = row_string(&buf, 0, 20);
        assert!(row.starts_with("hi"), "new content should render: {row:?}");
        // "CONTENT" sits well past the new "hi", entirely in the ghost region —
        // without the row clear it would survive and this would fail.
        assert!(!row.contains("CONTENT"), "old text must not ghost through: {row:?}");
        assert_eq!(row.trim_end(), "hi");
    }
}

#[cfg(test)]
mod reset_probe {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend, text::Line, widgets::{Paragraph, Widget}};

    struct Item(String);
    impl VirtualItem for Item {
        fn measure_height(&self, _w: u16) -> u16 { 1 }
        fn render(&self, area: Rect, buf: &mut Buffer, _s: bool) {
            Paragraph::new(Line::from(self.0.clone())).render(area, buf);
        }
        fn search_text(&self) -> String { self.0.clone() }
    }
    fn row0(t: &Terminal<TestBackend>) -> String {
        let b = t.backend().buffer();
        (0..b.area.width).map(|x| b.cell((x,0)).map(|c| c.symbol().to_string()).unwrap_or_default()).collect()
    }

    #[test]
    fn does_terminal_draw_reset_between_frames() {
        let mut t = Terminal::new(TestBackend::new(20, 3)).unwrap();
        let area = Rect::new(0,0,20,1);
        // Frame 1: long content
        t.draw(|f| {
            let mut l: VirtualList<Item> = VirtualList::new();
            l.sticky_bottom = false;
            l.set_items(vec![Item("LONGLINECONTENT1234".into())]);
            l.scroll_offset = 0;
            l.render(area, f.buffer_mut());
        }).unwrap();
        // Frame 2: SHORT content at the same row — if draw() resets, no ghost.
        t.draw(|f| {
            let mut l: VirtualList<Item> = VirtualList::new();
            l.sticky_bottom = false;
            l.set_items(vec![Item("hi".into())]);
            l.scroll_offset = 0;
            l.render(area, f.buffer_mut());
        }).unwrap();
        let r = row0(&t);
        eprintln!("ROW0 AFTER FRAME2 = {:?}", r);
        assert_eq!(r.trim_end(), "hi", "terminal.draw did NOT reset → ghost: {r:?}");
    }
}
