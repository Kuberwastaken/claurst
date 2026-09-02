//! Pointer input: mouse events, selection, context menu, paste viewer.

use crate::notifications::NotificationKind;
use crossterm::event::{KeyCode, KeyModifiers, MouseEvent, MouseEventKind};
use tracing::debug;
use super::App;
use super::types::{ContextMenuKind, ContextMenuItem, ContextMenuState, FocusTarget};

impl App {
    /// Detect if a click is a double-click based on timing and position.
    /// Returns true if the click is within ~500ms and ~5px of the last click.
    pub(super) fn is_double_click(&self, current_pos: (u16, u16)) -> bool {
        let now = std::time::Instant::now();
        match (self.last_click_time, self.last_click_position) {
            (Some(last_time), Some(last_pos)) => {
                let elapsed = now.duration_since(last_time);
                let distance = ((current_pos.0 as i32 - last_pos.0 as i32).abs()
                    + (current_pos.1 as i32 - last_pos.1 as i32).abs()) as u16;
                elapsed.as_millis() < 500 && distance <= 5
            }
            _ => false,
        }
    }

    /// Find word boundaries for the character at (col, row) in the rendered
    /// transcript buffer. Returns absolute (start_col, end_col) for the word
    /// containing the click. A "word" is a run of non-whitespace characters.
    pub(super) fn find_word_boundaries(&self, col: u16, row: u16) -> Option<(u16, u16)> {
        let cache = self.last_row_text.borrow();
        let line = cache.get(&row)?;
        if line.is_empty() {
            return None;
        }
        let selectable_area = self.last_selectable_area.get();
        if col < selectable_area.x {
            return None;
        }
        let local = (col - selectable_area.x) as usize;
        let chars: Vec<char> = line.chars().collect();
        if local >= chars.len() {
            return None;
        }
        let is_word = |c: char| !c.is_whitespace();
        if !is_word(chars[local]) {
            return None;
        }
        let mut start = local;
        while start > 0 && is_word(chars[start - 1]) {
            start -= 1;
        }
        let mut end = local;
        while end + 1 < chars.len() && is_word(chars[end + 1]) {
            end += 1;
        }
        Some((selectable_area.x + start as u16, selectable_area.x + end as u16))
    }

    /// Find paragraph boundaries (run of non-blank rows) around `row` and
    /// return (start_row, end_row, end_col) where end_col is the trimmed end
    /// of the last row's content. Used by triple-click selection so a
    /// "paragraph" — a contiguous block of text rows — is selected as a unit
    /// instead of a single visual row.
    pub(super) fn find_paragraph_boundaries(&self, row: u16) -> Option<(u16, u16, u16)> {
        let cache = self.last_row_text.borrow();
        let selectable_area = self.last_selectable_area.get();
        if selectable_area.width == 0 || selectable_area.height == 0 {
            return None;
        }
        let row_text = cache.get(&row)?;
        if row_text.trim().is_empty() {
            return None;
        }
        let max_row = selectable_area
            .y
            .saturating_add(selectable_area.height)
            .saturating_sub(1);
        let mut start = row;
        while start > selectable_area.y {
            let prev = start - 1;
            if cache.get(&prev).map(|s| s.trim().is_empty()).unwrap_or(true) {
                break;
            }
            start = prev;
        }
        let mut end = row;
        while end < max_row {
            let next = end + 1;
            if cache.get(&next).map(|s| s.trim().is_empty()).unwrap_or(true) {
                break;
            }
            end = next;
        }
        let last_text = cache.get(&end)?;
        let trimmed = last_text.trim_end();
        let end_col = selectable_area.x + trimmed.chars().count().saturating_sub(1) as u16;
        Some((start, end, end_col))
    }

    /// Find line boundaries for the row containing the click.
    /// Returns (start_row, end_row) for the line.
    #[allow(dead_code)]
    fn find_line_boundaries(&self, row: u16) -> Option<(u16, u16)> {
        let selectable_area = self.last_selectable_area.get();
        let line_start = selectable_area.y;
        let line_end = selectable_area.y.saturating_add(selectable_area.height).saturating_sub(1);

        if row >= line_start && row <= line_end {
            Some((row, row))
        } else {
            None
        }
    }

    pub(super) fn context_menu_items(kind: ContextMenuKind) -> &'static [ContextMenuItem] {
        match kind {
            ContextMenuKind::Message { .. } => &[ContextMenuItem::Copy, ContextMenuItem::Fork],
            ContextMenuKind::Selection => &[ContextMenuItem::Copy],
        }
    }

    pub(super) fn message_index_at_row(&self, row: u16) -> Option<usize> {
        self.message_row_map.borrow().get(&row).copied()
    }

    pub(super) fn clear_selection(&mut self) {
        self.selection_anchor = None;
        self.selection_focus = None;
        *self.selection_text.borrow_mut() = String::new();
    }

    /// Show context menu at the given position.
    pub(super) fn show_context_menu(&mut self, x: u16, y: u16, kind: ContextMenuKind) {
        self.context_menu_state = Some(ContextMenuState {
            x,
            y,
            selected_index: 0,
            kind,
        });
    }

    /// Dismiss the context menu.
    pub(super) fn dismiss_context_menu(&mut self) {
        self.context_menu_state = None;
    }

    /// Handle context menu navigation with arrow keys.
    pub(super) fn navigate_context_menu(&mut self, direction: KeyCode) {
        if let Some(mut menu) = self.context_menu_state {
            let item_count = Self::context_menu_items(menu.kind).len();
            if item_count == 0 {
                self.context_menu_state = Some(menu);
                return;
            }
            match direction {
                KeyCode::Up => {
                    if menu.selected_index == 0 {
                        menu.selected_index = item_count - 1;
                    } else {
                        menu.selected_index -= 1;
                    }
                }
                KeyCode::Down => {
                    menu.selected_index = (menu.selected_index + 1) % item_count;
                }
                _ => return,
            }
            self.context_menu_state = Some(menu);
        }
    }

    /// Execute the currently selected context menu item.
    pub(super) fn execute_context_menu_item(&mut self) {
        if let Some(menu) = self.context_menu_state {
            let items = Self::context_menu_items(menu.kind);

            if menu.selected_index < items.len() {
                let item = items[menu.selected_index];
                self.handle_context_menu_action(item, menu.kind);
            }
        }
        self.dismiss_context_menu();
    }

    /// Handle a context menu action.
    pub(super) fn handle_context_menu_action(&mut self, item: ContextMenuItem, kind: ContextMenuKind) {
        match item {
            ContextMenuItem::Copy => {
                let text = match kind {
                    ContextMenuKind::Message { message_index } => self
                        .messages
                        .get(message_index)
                        .map(|message| message.get_all_text()),
                    ContextMenuKind::Selection => {
                        let selected = self.selection_text.borrow().trim().to_string();
                        if selected.is_empty() {
                            None
                        } else {
                            Some(selected)
                        }
                    }
                };

                if let Some(text) = text {
                    if crate::message_copy::copy_to_clipboard(&text) {
                        self.push_notification(
                            NotificationKind::Info,
                            format!("Copied {} chars to clipboard.", text.len()),
                            Some(3),
                        );
                    } else {
                        self.push_notification(
                            NotificationKind::Warning,
                            "Failed to copy to clipboard.".to_string(),
                            Some(3),
                        );
                    }
                    debug!("Copy action triggered, text: {} chars", text.len());
                }
            }
            ContextMenuItem::Fork => {
                if let ContextMenuKind::Message { message_index } = kind {
                    let branch_point = message_index + 1;
                    self.prompt_input.replace_text(format!("/fork {}", branch_point));
                    self.status_message =
                        Some(format!("Fork at message {} - press Enter to confirm", branch_point));
                }
            }
        }
    }

    /// Process mouse events (trackpad scroll, text selection, etc.).
    /// Handle a left click inside the prompt input: move the cursor to the
    /// clicked position and, when the click lands on a `[Pasted text #N ...]`
    /// placeholder, expand it in place so the full pasted body can be read
    /// (and edited) before submitting.
    pub(super) fn handle_prompt_click(&mut self, col: u16, row: u16) {
        if self.prompt_input.text.is_empty() {
            return;
        }
        // Reconstruct the prompt widget geometry of the last rendered frame.
        // `last_input_area` is the whole bottom pane; `render_input` carves a
        // 1-row model/mode status line off the top when there is room, and
        // `render_prompt_input` adds an image-pill row when attachments are
        // pending, then a top separator row before the wrapped text rows.
        let mut rect = self.last_input_area.get();
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        if rect.height > 2 {
            rect.y += 1;
            rect.height -= 1;
        }
        if !self.prompt_input.pending_images.is_empty() && rect.height > 1 {
            rect.y += 1;
            rect.height -= 1;
        }
        // 2-cell "❯ " prefix + 2-cell right margin (see render_prompt_input).
        let width = rect.width.saturating_sub(4) as usize;
        if width == 0 {
            return;
        }
        let text_start_y = rect.y + 1; // top separator occupies rect.y
        let max_text_rows = rect.height.saturating_sub(2) as usize;
        let total_rows = self.prompt_input.visual_row_count(width);
        // Mirror the renderer's scroll: keep the cursor row visible.
        let (cursor_row, _) = self.prompt_input.cursor_visual_pos(width);
        let scroll = if total_rows > max_text_rows && cursor_row >= max_text_rows {
            cursor_row + 1 - max_text_rows
        } else {
            0
        };
        let visible_rows = total_rows.saturating_sub(scroll).min(max_text_rows);
        if row < text_start_y || (row - text_start_y) as usize >= visible_rows {
            return;
        }
        let target_row = scroll + (row - text_start_y) as usize;
        let target_col = col.saturating_sub(rect.x + 2) as usize;
        self.prompt_input.set_cursor_at_visual(target_row, target_col, width);
        // Clicking a [Pasted text #N ...] placeholder opens the read-only
        // viewer so the body can be read without splicing it into the
        // prompt; Alt+E remains the in-place expansion for editing.
        if let Some((id, body)) = self.prompt_input.paste_ref_at(self.prompt_input.cursor) {
            self.paste_viewer.open(id, &body);
        }
        self.refresh_prompt_input();
    }

    /// Key handling while the paste viewer modal is open.
    pub(super) fn handle_paste_viewer_key(&mut self, key: crossterm::event::KeyEvent) {
        use crossterm::event::{KeyCode, KeyModifiers};
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.paste_viewer.close(),
            KeyCode::Up | KeyCode::Char('k') => self.paste_viewer.scroll_up(1),
            KeyCode::Down | KeyCode::Char('j') => self.paste_viewer.scroll_down(1),
            KeyCode::PageUp => self.paste_viewer.page_up(),
            KeyCode::PageDown => self.paste_viewer.page_down(),
            KeyCode::Home | KeyCode::Char('g') => self.paste_viewer.scroll_to_top(),
            KeyCode::End | KeyCode::Char('G') => self.paste_viewer.scroll_to_bottom(),
            // Alt+E from inside the viewer: same in-place expansion as on the
            // placeholder itself, then close (the body now lives in the
            // prompt buffer).
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::ALT) => {
                let id = self.paste_viewer.paste_id;
                self.paste_viewer.close();
                self.expand_paste_ref_by_id(id);
            }
            _ => {}
        }
    }

    /// Expand the `[Pasted text #N ...]` placeholder with the given id, if it
    /// is still present in the prompt buffer with a stored body.
    pub(super) fn expand_paste_ref_by_id(&mut self, id: u32) {
        let target =
            claurst_core::prompt_history::parse_references_with_positions(&self.prompt_input.text)
                .into_iter()
                .find(|(rid, matched, _)| *rid == id && matched.starts_with("[Pasted text #"));
        if let Some((_, _, start)) = target {
            self.prompt_input.expand_paste_ref_at(start);
            self.refresh_prompt_input();
        }
    }

    pub fn handle_mouse_event(&mut self, mouse_event: MouseEvent) {
        use crossterm::event::MouseButton;

        // When mouse capture is disabled (mouseCapture: false, issue #104) the
        // terminal keeps the mouse for native click-drag selection / copy-paste,
        // so the app must not act on any mouse events that still slip through.
        // Keyboard scrolling (PageUp/PageDown, etc.) is handled elsewhere and is
        // unaffected by this gate.
        if !self.config.mouse_capture_enabled() {
            return;
        }

        // The paste viewer modal swallows mouse input: the wheel scrolls its
        // body, everything else is inert (Esc/q close it).
        if self.paste_viewer.visible {
            match mouse_event.kind {
                MouseEventKind::ScrollUp => self.paste_viewer.scroll_up(3),
                MouseEventKind::ScrollDown => self.paste_viewer.scroll_down(3),
                _ => {}
            }
            return;
        }

        // Fast-reject mouse-move events — they flood at 60+ Hz and we don't
        // need hover tracking. Exception: context menu needs hover to update
        // the selected item highlight.
        if matches!(mouse_event.kind, MouseEventKind::Moved) {
            if let Some(menu) = self.context_menu_state.as_mut() {
                let items = Self::context_menu_items(menu.kind);
                let item_labels: Vec<&str> = items.iter().map(|i| match i {
                    ContextMenuItem::Copy => "Copy",
                    ContextMenuItem::Fork => "Fork new chat",
                }).collect();
                let menu_width = (item_labels.iter().map(|l| l.len()).max().unwrap_or(4) + 4) as u16;
                let menu_height = items.len() as u16 + 2;
                let screen = self.last_msg_area.get();
                let menu_x = menu.x.min(screen.x.saturating_add(screen.width).saturating_sub(menu_width + 1));
                let menu_y = menu.y.min(screen.y.saturating_add(screen.height).saturating_sub(menu_height + 1));
                let inner_y = menu_y + 1;
                let col = mouse_event.column;
                let row = mouse_event.row;
                if col >= menu_x
                    && col < menu_x.saturating_add(menu_width)
                    && row >= inner_y
                    && row < inner_y.saturating_add(items.len() as u16)
                {
                    let hovered = (row - inner_y) as usize;
                    if hovered < items.len() {
                        menu.selected_index = hovered;
                    }
                }
            }
            return;
        }

        // ---- Dialog interaction: dismiss on click-outside, scroll/click inside ----
        // Key-input and device-auth stay outside this gate so their visible text
        // can still be selected and copied with the mouse.
        let any_dialog = self.connect_dialog.visible
            || self.import_config_picker.visible
            || self.import_config_dialog.visible
            || self.command_palette.visible
            || self.model_picker.visible
            || self.export_dialog.visible
            || self.settings_screen.visible
            || self.stats_dialog.visible
            || self.context_viz.visible
            || self.session_browser.visible;

        if any_dialog {
            match mouse_event.kind {
                MouseEventKind::Down(MouseButton::Left) => {
                    // DialogSelect dialogs — check if click is inside for item selection
                    let in_dialog = if self.connect_dialog.visible {
                        self.connect_dialog.contains(mouse_event.column, mouse_event.row)
                    } else if self.import_config_picker.visible {
                        self.import_config_picker.contains(mouse_event.column, mouse_event.row)
                    } else if self.command_palette.visible {
                        self.command_palette.contains(mouse_event.column, mouse_event.row)
                    } else {
                        // Other dialogs (model_picker, settings, export, etc.) —
                        // treat any click as "inside" to prevent accidental dismiss.
                        // User must press Esc to close these.
                        true
                    };

                    if in_dialog {
                        // Click inside a DialogSelect — select the clicked item
                        if self.connect_dialog.visible {
                            self.connect_dialog.handle_mouse_click(mouse_event.row);
                        } else if self.import_config_picker.visible {
                            self.import_config_picker.handle_mouse_click(mouse_event.row);
                        } else if self.command_palette.visible {
                            self.command_palette.handle_mouse_click(mouse_event.row);
                        }
                        // Other dialogs: click absorbed, no action needed
                    } else {
                        // Click outside a DialogSelect — dismiss and restore input focus
                        self.close_secondary_views();
                        self.focus = FocusTarget::Input;
                    }
                }
                MouseEventKind::ScrollUp => {
                    // Scroll through dialog items
                    if self.connect_dialog.visible { self.connect_dialog.move_up(); }
                    else if self.import_config_picker.visible { self.import_config_picker.move_up(); }
                    else if self.command_palette.visible { self.command_palette.move_up(); }
                }
                MouseEventKind::ScrollDown => {
                    if self.connect_dialog.visible { self.connect_dialog.move_down(); }
                    else if self.import_config_picker.visible { self.import_config_picker.move_down(); }
                    else if self.command_palette.visible { self.command_palette.move_down(); }
                }
                _ => {}
            }
            return; // Don't process any other mouse events when a dialog is open
        }

        match mouse_event.kind {
            MouseEventKind::ScrollUp => {
                // Don't consume Ctrl+Scroll — let the terminal handle zoom.
                if !mouse_event.modifiers.contains(KeyModifiers::CONTROL) {
                    let step = self.scroll_step();
                    self.scroll_up_by(step);
                }
            }
            MouseEventKind::ScrollDown => {
                if !mouse_event.modifiers.contains(KeyModifiers::CONTROL) {
                    let step = self.scroll_step();
                    let new_off = self.scroll_offset.saturating_sub(step);
                    self.scroll_offset = new_off;
                    if new_off == 0 {
                        self.auto_scroll = true;
                        self.new_messages_while_scrolled = 0;
                    }
                }
            }
            // ---- Right-click context menu ----------------------------------
            MouseEventKind::Down(MouseButton::Right) => {
                let msg_area = self.last_msg_area.get();
                let has_selection = !self.selection_text.borrow().trim().is_empty();
                if mouse_event.column >= msg_area.x
                    && mouse_event.column < msg_area.x.saturating_add(msg_area.width)
                    && mouse_event.row >= msg_area.y
                    && mouse_event.row < msg_area.y.saturating_add(msg_area.height)
                {
                    if let Some(message_index) = self.message_index_at_row(mouse_event.row) {
                        self.show_context_menu(
                            mouse_event.column,
                            mouse_event.row,
                            ContextMenuKind::Message { message_index },
                        );
                    } else {
                        self.dismiss_context_menu();
                    }
                } else if has_selection {
                    self.show_context_menu(
                        mouse_event.column,
                        mouse_event.row,
                        ContextMenuKind::Selection,
                    );
                } else {
                    self.dismiss_context_menu();
                }
            }

            // ---- Primary-selection paste into the prompt ---------------
            MouseEventKind::Down(MouseButton::Middle) => {
                let _ = self.paste_primary_into_prompt();
            }

            // ---- Text selection / focus routing -------------------------
            MouseEventKind::Down(MouseButton::Left) => {
                // If a context menu is open, check if the click is on a menu item.
                // Must replicate the same position clamping as the renderer.
                if let Some(menu) = self.context_menu_state {
                    let items = Self::context_menu_items(menu.kind);
                    let item_labels: Vec<&str> = items.iter().map(|i| match i {
                        ContextMenuItem::Copy => "Copy",
                        ContextMenuItem::Fork => "Fork new chat",
                    }).collect();
                    let menu_width = (item_labels.iter().map(|l| l.len()).max().unwrap_or(4) + 4) as u16;
                    let menu_height = items.len() as u16 + 2; // +2 for border
                    // Clamp to screen bounds (same as render_context_menu)
                    let screen = self.last_msg_area.get();
                    let menu_x = menu.x.min(screen.x.saturating_add(screen.width).saturating_sub(menu_width + 1));
                    let menu_y = menu.y.min(screen.y.saturating_add(screen.height).saturating_sub(menu_height + 1));
                    let col = mouse_event.column;
                    let row = mouse_event.row;
                    // Inner area starts 1 past the border
                    let inner_y = menu_y + 1;
                    if col >= menu_x
                        && col < menu_x.saturating_add(menu_width)
                        && row >= inner_y
                        && row < inner_y.saturating_add(items.len() as u16)
                    {
                        let clicked_index = (row - inner_y) as usize;
                        if clicked_index < items.len() {
                            self.context_menu_state.as_mut().unwrap().selected_index = clicked_index;
                            self.execute_context_menu_item();
                            return;
                        }
                    }
                    // Click was outside the menu — just dismiss it
                    self.dismiss_context_menu();
                    return;
                }

                let input_area = self.last_input_area.get();
                let selectable_area = self.last_selectable_area.get();

                let in_input = input_area.width > 0 && input_area.height > 0
                    && mouse_event.row >= input_area.y
                    && mouse_event.row < input_area.y.saturating_add(input_area.height)
                    && mouse_event.column >= input_area.x
                    && mouse_event.column < input_area.x.saturating_add(input_area.width);

                let in_selectable = selectable_area.width > 0 && selectable_area.height > 0
                    && mouse_event.row >= selectable_area.y
                    && mouse_event.row < selectable_area.y.saturating_add(selectable_area.height)
                    && mouse_event.column >= selectable_area.x
                    && mouse_event.column < selectable_area.x.saturating_add(selectable_area.width);

                // Check for click on a thinking block header (takes priority over text selection).
                if let Some(&hash) = self.thinking_row_map.borrow().get(&mouse_event.row) {
                    if self.thinking_expanded.contains(&hash) {
                        self.thinking_expanded.remove(&hash);
                    } else {
                        self.thinking_expanded.insert(hash);
                    }
                    self.invalidate_transcript();
                    return;
                }

                if in_input {
                    self.focus = FocusTarget::Input;
                    self.clear_selection();
                    self.handle_prompt_click(mouse_event.column, mouse_event.row);
                } else if selectable_area.width == 0 || selectable_area.height == 0 {
                    self.click_count = 0;
                } else if in_selectable {
                    self.focus = FocusTarget::Transcript;

                    let current_pos = (mouse_event.column, mouse_event.row);
                    let now = std::time::Instant::now();

                    // Check for double-click
                    if self.is_double_click(current_pos) {
                        self.click_count += 1;
                        if self.click_count >= 3 {
                            // Triple-click: select the paragraph (run of
                            // non-blank rows) containing the click. Falls back
                            // to a single line if no paragraph is detected.
                            if let Some((start_row, end_row, end_col)) =
                                self.find_paragraph_boundaries(current_pos.1)
                            {
                                self.selection_anchor = Some((selectable_area.x, start_row));
                                self.selection_focus = Some((end_col, end_row));
                            } else {
                                self.selection_anchor = Some((selectable_area.x, current_pos.1));
                                self.selection_focus = Some((
                                    selectable_area
                                        .x
                                        .saturating_add(selectable_area.width)
                                        .saturating_sub(1),
                                    current_pos.1,
                                ));
                            }
                            self.click_count = 0; // Reset for next click sequence
                        } else {
                            // Double-click: select word
                            if let Some((start, end)) = self.find_word_boundaries(current_pos.0, current_pos.1) {
                                self.selection_anchor = Some((start, current_pos.1));
                                self.selection_focus = Some((end, current_pos.1));
                            }
                        }
                    } else {
                        // Single click or new click sequence
                        self.click_count = 1;
                        self.selection_anchor = Some(current_pos);
                        self.selection_focus = Some(current_pos);
                        *self.selection_text.borrow_mut() = String::new();
                    }

                    self.last_click_time = Some(now);
                    self.last_click_position = Some(current_pos);
                } else {
                    self.click_count = 0;
                    self.clear_selection();
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                // Dismiss context menu on drag
                self.dismiss_context_menu();

                // Continue drag — clamp to the selectable frame bounds so dragging
                // outside extends selection to the edge rather than cancelling.
                if self.selection_anchor.is_some() {
                    let selectable_area = self.last_selectable_area.get();
                    if selectable_area.width > 0 && selectable_area.height > 0 {
                        let clamped_col = mouse_event.column
                            .max(selectable_area.x)
                            .min(selectable_area.x.saturating_add(selectable_area.width).saturating_sub(1));
                        let clamped_row = mouse_event.row
                            .max(selectable_area.y)
                            .min(selectable_area.y.saturating_add(selectable_area.height).saturating_sub(1));
                        self.selection_focus = Some((clamped_col, clamped_row));
                        self.click_count = 0; // Reset on drag to prevent further double-clicks
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                // Clear if no actual drag (single click = no selection)
                if self.selection_anchor == self.selection_focus {
                    self.clear_selection();
                } else if self.settings_screen.auto_copy_enabled {
                    // Auto-copy finalized selection to clipboard.
                    let sel_text = self.selection_text.borrow().clone();
                    if !sel_text.is_empty() {
                        let copied = crate::image_paste::write_clipboard_text(&sel_text);
                        if copied {
                            self.push_notification(
                                NotificationKind::Info,
                                "Copied to clipboard".to_string(),
                                Some(1),
                            );
                        }
                    }
                }
            }
            _ => {}
        }
    }

}
