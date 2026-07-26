//! Complete PromptInput — multi-line text editor for the TUI.
//! Mirrors src/components/PromptInput/ (21 files) and src/vim/ (5 files).
//!
//! Features:
//! - Multi-line editing (Shift+Enter for newlines)
//! - Vim Normal/Insert/Visual modes
//! - History navigation (↑↓ through history.jsonl)
//! - Slash command typeahead
//! - Paste handling (large pastes → placeholder)
//! - Character count + token estimate

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const CLAUDE_ORANGE: Color = Color::Rgb(233, 30, 99);
const PROMPT_POINTER: &str = "\u{276f}";

// ---------------------------------------------------------------------------
// Vim mode
// ---------------------------------------------------------------------------

/// Vim editor mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VimMode {
    #[default]
    Insert,
    Normal,
    Visual,
    /// Linewise visual selection (V).
    VisualLine,
    /// Block visual selection (Ctrl+V).
    VisualBlock,
    /// Command-line mode (:).
    Command,
    /// In-prompt forward search (/).
    Search,
}

impl VimMode {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Insert => "INSERT",
            Self::Normal => "NORMAL",
            Self::Visual => "VISUAL",
            Self::VisualLine => "VISUAL LINE",
            Self::VisualBlock => "VISUAL BLOCK",
            Self::Command => "COMMAND",
            Self::Search => "SEARCH",
        }
    }

    pub fn color(&self) -> Color {
        match self {
            Self::Insert => Color::Blue,
            Self::Normal => Color::Green,
            Self::Visual | Self::VisualLine | Self::VisualBlock => Color::Magenta,
            Self::Command | Self::Search => Color::Cyan,
        }
    }
}

// ---------------------------------------------------------------------------
// Extended vim state types (full state machine)
// ---------------------------------------------------------------------------

/// Pending multi-key vim command state.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum VimPendingState {
    #[default]
    None,
    /// Accumulating count digits before a command (e.g. `3` before `w`).
    Count { digits: String },
    /// Received `g`, waiting for second key.
    G { count: usize },
    /// Received operator (d/c/y), waiting for motion.
    Operator { op: VimOperator, count: usize },
    /// Received operator then additional count digits.
    OperatorCount { op: VimOperator, count: usize, digits: String },
    /// Received `dg`/`cg`/`yg`, waiting for second g key.
    OperatorG { op: VimOperator, count: usize },
    /// Received `f/F/t/T`, waiting for target char.
    Find { kind: VimFindKind, count: usize },
    /// Received `r`, waiting for replacement char.
    Replace { count: usize },
    /// Received `>` or `<`, waiting for second `>` or `<`.
    Indent { dir: char, count: usize },
    /// Received `"`, waiting for register name char.
    Register(char),
    /// After `"reg`, waiting for operator (y/d/p).
    RegisterOp(char),
    /// Received `m`, waiting for mark name char.
    Mark,
    /// Received `'`, waiting for mark name char for jump.
    JumpMark,
    /// Received `q`, waiting for register char to record into.
    MacroRecord,
    /// Received `@`, waiting for register char to replay.
    MacroReplay,
}

/// Vim operator type used with motion + operator combos.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimOperator {
    Delete,
    Change,
    Yank,
    /// Uppercase region (gU).
    Uppercase,
    /// Lowercase region (gu).
    Lowercase,
}

/// Vim character-find direction and variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VimFindKind {
    /// `f{c}` — forward, cursor lands on char
    F,
    /// `F{c}` — backward, cursor lands on char
    BigF,
    /// `t{c}` — forward, cursor stops before char
    T,
    /// `T{c}` — backward, cursor stops after char
    BigT,
}

/// Stores enough information to replay the last modifying vim command (`.`).
#[derive(Clone, Debug)]
pub enum DotRepeatAction {
    /// Insert text at current cursor (from i, a, A, o, O, s).
    Insert { text: String, mode_after_insert: bool },
    /// Simplified: re-delete the same number of chars.
    DeleteChars { count: usize },
    /// Change: delete + insert.
    Change { deleted: String, inserted: String },
    /// Replace char.
    ReplaceChar { ch: char },
}

// ---------------------------------------------------------------------------
// Motion helper functions (byte-safe, work on UTF-8 byte offsets)
// ---------------------------------------------------------------------------

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Convert a char-index within `text` to a byte offset.
fn char_idx_to_byte(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(text.len())
}

/// `w` — start of next word.
fn motion_w(text: &str, cursor: usize) -> usize {
    let rest = &text[cursor..];
    let chars: Vec<char> = rest.chars().collect();
    let n = chars.len();
    if n == 0 { return cursor; }
    let mut i = 0;
    if is_word_char(chars[0]) {
        while i < n && is_word_char(chars[i]) { i += 1; }
    } else if !chars[0].is_whitespace() {
        while i < n && !is_word_char(chars[i]) && !chars[i].is_whitespace() { i += 1; }
    }
    while i < n && chars[i].is_whitespace() { i += 1; }
    cursor + char_idx_to_byte(rest, i)
}

/// `b` — start of previous word.
fn motion_b(text: &str, cursor: usize) -> usize {
    if cursor == 0 { return 0; }
    let before = &text[..cursor];
    let chars: Vec<char> = before.chars().collect();
    let n = chars.len();
    if n == 0 { return 0; }
    let mut i = n;
    while i > 0 && chars[i - 1].is_whitespace() { i -= 1; }
    if i == 0 { return 0; }
    if is_word_char(chars[i - 1]) {
        while i > 0 && is_word_char(chars[i - 1]) { i -= 1; }
    } else {
        while i > 0 && !is_word_char(chars[i - 1]) && !chars[i - 1].is_whitespace() { i -= 1; }
    }
    char_idx_to_byte(before, i)
}

/// `e` — end of current/next word.
fn motion_e(text: &str, cursor: usize) -> usize {
    let chars: Vec<(usize, char)> = text[cursor..]
        .char_indices()
        .map(|(b, c)| (cursor + b, c))
        .collect();
    let n = chars.len();
    if n == 0 { return cursor; }
    let at_end = n == 1
        || chars[1].1.is_whitespace()
        || is_word_char(chars[0].1) != is_word_char(chars[1].1);
    let mut i = 0;
    if at_end {
        i = 1;
        while i < n && chars[i].1.is_whitespace() { i += 1; }
    }
    if i >= n { return cursor; }
    let wc = is_word_char(chars[i].1);
    while i + 1 < n && !chars[i + 1].1.is_whitespace() && is_word_char(chars[i + 1].1) == wc {
        i += 1;
    }
    chars[i].0
}

/// `W` — start of next WORD (any non-whitespace run).
#[allow(non_snake_case)]
fn motion_W(text: &str, cursor: usize) -> usize {
    let rest = &text[cursor..];
    let chars: Vec<char> = rest.chars().collect();
    let n = chars.len();
    if n == 0 { return cursor; }
    let mut i = 0;
    while i < n && !chars[i].is_whitespace() { i += 1; }
    while i < n && chars[i].is_whitespace() { i += 1; }
    cursor + char_idx_to_byte(rest, i)
}

/// `B` — start of previous WORD.
#[allow(non_snake_case)]
fn motion_B(text: &str, cursor: usize) -> usize {
    if cursor == 0 { return 0; }
    let before = &text[..cursor];
    let chars: Vec<char> = before.chars().collect();
    let n = chars.len();
    let mut i = n;
    while i > 0 && chars[i - 1].is_whitespace() { i -= 1; }
    while i > 0 && !chars[i - 1].is_whitespace() { i -= 1; }
    char_idx_to_byte(before, i)
}

/// `E` — end of current/next WORD.
#[allow(non_snake_case)]
fn motion_E(text: &str, cursor: usize) -> usize {
    let chars: Vec<(usize, char)> = text[cursor..]
        .char_indices()
        .map(|(b, c)| (cursor + b, c))
        .collect();
    let n = chars.len();
    if n == 0 { return cursor; }
    let at_end = n == 1 || chars[1].1.is_whitespace();
    let mut i = 0;
    if at_end {
        i = 1;
        while i < n && chars[i].1.is_whitespace() { i += 1; }
    }
    if i >= n { return cursor; }
    while i + 1 < n && !chars[i + 1].1.is_whitespace() { i += 1; }
    chars[i].0
}

/// `^` — first non-blank character on the current line.
fn motion_first_nonblank(text: &str, cursor: usize) -> usize {
    let line_start = text[..cursor].rfind('\n').map(|p| p + 1).unwrap_or(0);
    let rest = &text[line_start..];
    let skip_bytes = rest
        .char_indices()
        .take_while(|(_, c)| *c == ' ' || *c == '\t')
        .last()
        .map(|(b, c)| b + c.len_utf8())
        .unwrap_or(0);
    line_start + skip_bytes
}

/// `G` — first char of the last line.
#[allow(non_snake_case)]
fn motion_G(text: &str) -> usize {
    text.rfind('\n').map(|p| p + 1).unwrap_or(0)
}

/// `gg` / line-N — go to start of line `line_num` (1-indexed; 0 or 1 → start of text).
fn motion_gg(text: &str, line_num: usize) -> usize {
    if line_num <= 1 { return 0; }
    let mut line = 1usize;
    for (b, c) in text.char_indices() {
        if c == '\n' {
            line += 1;
            if line == line_num {
                return b + 1;
            }
        }
    }
    text.rfind('\n').map(|p| p + 1).unwrap_or(0)
}

/// `f/F/t/T{char}` — find character in text. Returns new cursor byte offset.
fn motion_find_char(
    text: &str,
    cursor: usize,
    target: char,
    kind: VimFindKind,
    count: usize,
) -> Option<usize> {
    match kind {
        VimFindKind::F | VimFindKind::T => {
            let search_start = text[cursor..].char_indices().nth(1).map(|(b, _)| cursor + b)?;
            let mut hits = 0usize;
            for (b, c) in text[search_start..].char_indices() {
                if c == target {
                    hits += 1;
                    if hits == count {
                        let pos = search_start + b;
                        if matches!(kind, VimFindKind::T) {
                            return text[cursor..pos]
                                .char_indices()
                                .last()
                                .map(|(lb, _)| cursor + lb);
                        }
                        return Some(pos);
                    }
                }
            }
            None
        }
        VimFindKind::BigF | VimFindKind::BigT => {
            let before = &text[..cursor];
            let mut hits = 0usize;
            for (b, c) in before.char_indices().rev() {
                if c == target {
                    hits += 1;
                    if hits == count {
                        if matches!(kind, VimFindKind::BigT) {
                            return text[b..].char_indices().nth(1).map(|(nb, _)| b + nb).or(Some(cursor));
                        }
                        return Some(b);
                    }
                }
            }
            None
        }
    }
}

/// Convert text region to uppercase.
fn uppercase_region(text: &str) -> String {
    text.chars().map(|c| {
        c.to_uppercase().next().unwrap_or(c)
    }).collect()
}

/// Convert text region to lowercase.
fn lowercase_region(text: &str) -> String {
    text.chars().map(|c| {
        c.to_lowercase().next().unwrap_or(c)
    }).collect()
}

/// Apply an operator (d/c/y/gU/gu) to the range [from, to) in text.
/// Returns `(new_text, new_cursor)`. For Change, sets mode to Insert.
fn apply_operator_range(
    op: VimOperator,
    text: &str,
    from: usize,
    to: usize,
    yank_buf: &mut String,
    mode: &mut VimMode,
) -> (String, usize) {
    let to = to.min(text.len());
    let from = from.min(to);
    let selected = &text[from..to];
    *yank_buf = selected.to_string();
    match op {
        VimOperator::Yank => (text.to_string(), from),
        VimOperator::Delete => {
            let new_text = format!("{}{}", &text[..from], &text[to..]);
            let new_cursor = from.min(new_text.len().saturating_sub(if new_text.is_empty() { 0 } else { 1 }));
            (new_text, new_cursor)
        }
        VimOperator::Change => {
            let new_text = format!("{}{}", &text[..from], &text[to..]);
            *mode = VimMode::Insert;
            (new_text, from)
        }
        VimOperator::Uppercase => {
            let upper = uppercase_region(selected);
            let new_text = format!("{}{}{}", &text[..from], upper, &text[to..]);
            (new_text, from)
        }
        VimOperator::Lowercase => {
            let lower = lowercase_region(selected);
            let new_text = format!("{}{}{}", &text[..from], lower, &text[to..]);
            (new_text, from)
        }
    }
}

// ---------------------------------------------------------------------------
// Full vim key handler (state machine)
// ---------------------------------------------------------------------------

/// Process a single key press in vim mode.
/// Returns `true` when text was modified (caller should push undo snapshot).
pub fn apply_vim_key(
    mode: &mut VimMode,
    text: &mut String,
    cursor: &mut usize,
    key: &str,
    yank_buf: &mut String,
    pending: &mut VimPendingState,
    last_find: &mut Option<(VimFindKind, char)>,
) -> bool {
    // Escape always cancels pending state and returns to Normal
    if key == "Escape" {
        *mode = VimMode::Normal;
        *pending = VimPendingState::None;
        return false;
    }

    match std::mem::replace(pending, VimPendingState::None) {
        VimPendingState::None => {
            vim_idle(mode, text, cursor, key, yank_buf, pending, last_find)
        }
        VimPendingState::Count { digits } => {
            vim_count(mode, text, cursor, key, yank_buf, pending, last_find, digits)
        }
        VimPendingState::G { count } => {
            vim_g(text, cursor, key, pending, count)
        }
        VimPendingState::Operator { op, count } => {
            vim_operator(mode, text, cursor, key, yank_buf, pending, last_find, op, count)
        }
        VimPendingState::OperatorCount { op, count, digits } => {
            vim_operator_count(mode, text, cursor, key, yank_buf, pending, last_find, op, count, digits)
        }
        VimPendingState::OperatorG { op, count } => {
            vim_operator_g(mode, text, cursor, key, yank_buf, op, count)
        }
        VimPendingState::Find { kind, count } => {
            if key.len() == 1 {
                let c = key.chars().next().unwrap();
                if let Some(new_pos) = motion_find_char(text, *cursor, c, kind, count) {
                    *cursor = new_pos;
                    *last_find = Some((kind, c));
                }
            }
            false
        }
        VimPendingState::Replace { count } => {
            if key.len() == 1 {
                let c = key.chars().next().unwrap();
                let mut modified = false;
                let mut pos = *cursor;
                for _ in 0..count.max(1) {
                    if pos >= text.len() { break; }
                    let clen = text[pos..].chars().next().map(|ch| ch.len_utf8()).unwrap_or(1);
                    text.replace_range(pos..pos + clen, &c.to_string());
                    pos += c.len_utf8();
                    modified = true;
                }
                *cursor = (*cursor).min(text.len().saturating_sub(if text.is_empty() { 0 } else { 1 }));
                modified
            } else {
                false
            }
        }
        VimPendingState::Indent { dir, count } => {
            if key == dir.to_string().as_str() {
                let indent = "  ";
                let current_line = text[..*cursor].chars().filter(|&c| c == '\n').count();
                let mut new_lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
                for i in 0..count.max(1) {
                    let idx = current_line + i;
                    if idx >= new_lines.len() { break; }
                    if dir == '>' {
                        new_lines[idx] = format!("{}{}", indent, new_lines[idx]);
                    } else if new_lines[idx].starts_with(indent) {
                        new_lines[idx] = new_lines[idx][indent.len()..].to_string();
                    } else {
                        let trimmed = new_lines[idx].trim_start_matches('\t').trim_start_matches(' ');
                        new_lines[idx] = trimmed.to_string();
                    }
                }
                *text = new_lines.join("\n");
                *cursor = (*cursor).min(text.len());
                true
            } else {
                false
            }
        }
        // These pending states are fully handled in PromptInputState::vim_command
        // before apply_vim_key is called, but we need arms for exhaustiveness.
        VimPendingState::Register(_)
        | VimPendingState::RegisterOp(_)
        | VimPendingState::Mark
        | VimPendingState::JumpMark
        | VimPendingState::MacroRecord
        | VimPendingState::MacroReplay => false,
    }
}

fn vim_idle(
    mode: &mut VimMode,
    text: &mut String,
    cursor: &mut usize,
    key: &str,
    yank_buf: &mut String,
    pending: &mut VimPendingState,
    last_find: &mut Option<(VimFindKind, char)>,
) -> bool {
    // Count prefix (1-9 only; 0 is the line-start motion)
    if key.len() == 1 {
        let ch = key.chars().next().unwrap();
        if ch.is_ascii_digit() && ch != '0' {
            *pending = VimPendingState::Count { digits: key.to_string() };
            return false;
        }
    }
    vim_normal(mode, text, cursor, key, yank_buf, pending, last_find, 1)
}

fn vim_count(
    mode: &mut VimMode,
    text: &mut String,
    cursor: &mut usize,
    key: &str,
    yank_buf: &mut String,
    pending: &mut VimPendingState,
    last_find: &mut Option<(VimFindKind, char)>,
    digits: String,
) -> bool {
    if key.len() == 1 && key.chars().next().unwrap().is_ascii_digit() {
        let new_digits = format!("{}{}", digits, key);
        let count: usize = new_digits.parse().unwrap_or(10000).min(10000);
        *pending = VimPendingState::Count { digits: count.to_string() };
        return false;
    }
    let count: usize = digits.parse().unwrap_or(1);
    vim_normal(mode, text, cursor, key, yank_buf, pending, last_find, count)
}

#[allow(clippy::too_many_arguments)]
fn vim_normal(
    mode: &mut VimMode,
    text: &mut String,
    cursor: &mut usize,
    key: &str,
    yank_buf: &mut String,
    pending: &mut VimPendingState,
    last_find: &mut Option<(VimFindKind, char)>,
    count: usize,
) -> bool {
    let n = count.max(1);
    match key {
        // ---- Mode transitions ----
        "i" => { *mode = VimMode::Insert; false }
        "a" => {
            *mode = VimMode::Insert;
            if *cursor < text.len() {
                *cursor = text[*cursor..].char_indices().nth(1).map(|(b, _)| *cursor + b).unwrap_or(text.len());
            }
            false
        }
        "I" => { *mode = VimMode::Insert; *cursor = motion_first_nonblank(text, *cursor); false }
        "A" => {
            *mode = VimMode::Insert;
            *cursor = text[*cursor..].find('\n').map(|p| *cursor + p).unwrap_or(text.len());
            false
        }
        "v" => { *mode = VimMode::Visual; false }
        // ---- Simple motions ----
        "h" => {
            for _ in 0..n {
                if *cursor > 0 {
                    let prev = text[..*cursor].char_indices().last().map(|(b, _)| b).unwrap_or(0);
                    *cursor = prev;
                }
            }
            false
        }
        "l" => {
            for _ in 0..n {
                if *cursor < text.len() {
                    *cursor = text[*cursor..].char_indices().nth(1).map(|(b, _)| *cursor + b).unwrap_or(text.len());
                }
            }
            false
        }
        "0" => { *cursor = text[..*cursor].rfind('\n').map(|p| p + 1).unwrap_or(0); false }
        "^" => { *cursor = motion_first_nonblank(text, *cursor); false }
        "$" => { *cursor = text[*cursor..].find('\n').map(|p| *cursor + p).unwrap_or(text.len()); false }
        "w" => { for _ in 0..n { *cursor = motion_w(text, *cursor); } false }
        "b" => { for _ in 0..n { *cursor = motion_b(text, *cursor); } false }
        "e" => { for _ in 0..n { *cursor = motion_e(text, *cursor); } false }
        "W" => { for _ in 0..n { *cursor = motion_W(text, *cursor); } false }
        "B" => { for _ in 0..n { *cursor = motion_B(text, *cursor); } false }
        "E" => { for _ in 0..n { *cursor = motion_E(text, *cursor); } false }
        "G" => {
            *cursor = if n == 1 { motion_G(text) } else { motion_gg(text, n) };
            false
        }
        "g" => { *pending = VimPendingState::G { count: n }; false }
        // ---- Find motions ----
        "f" => { *pending = VimPendingState::Find { kind: VimFindKind::F, count: n }; false }
        "F" => { *pending = VimPendingState::Find { kind: VimFindKind::BigF, count: n }; false }
        "t" => { *pending = VimPendingState::Find { kind: VimFindKind::T, count: n }; false }
        "T" => { *pending = VimPendingState::Find { kind: VimFindKind::BigT, count: n }; false }
        ";" => {
            if let Some((kind, c)) = *last_find {
                if let Some(pos) = motion_find_char(text, *cursor, c, kind, n) { *cursor = pos; }
            }
            false
        }
        "," => {
            if let Some((kind, c)) = *last_find {
                let rev = match kind {
                    VimFindKind::F => VimFindKind::BigF, VimFindKind::BigF => VimFindKind::F,
                    VimFindKind::T => VimFindKind::BigT, VimFindKind::BigT => VimFindKind::T,
                };
                if let Some(pos) = motion_find_char(text, *cursor, c, rev, n) { *cursor = pos; }
            }
            false
        }
        // ---- Operators ----
        "d" => { *pending = VimPendingState::Operator { op: VimOperator::Delete, count: n }; false }
        "c" => { *pending = VimPendingState::Operator { op: VimOperator::Change, count: n }; false }
        "y" => { *pending = VimPendingState::Operator { op: VimOperator::Yank, count: n }; false }
        // ---- Single-char delete/change shortcuts ----
        "x" => {
            if *cursor < text.len() {
                let clen = text[*cursor..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
                *yank_buf = text[*cursor..*cursor + clen].to_string();
                text.drain(*cursor..*cursor + clen);
                *cursor = (*cursor).min(text.len().saturating_sub(if text.is_empty() { 0 } else { 1 }));
                return true;
            }
            false
        }
        "X" => {
            if *cursor > 0 {
                let prev = text[..*cursor].char_indices().last().map(|(b, _)| b).unwrap_or(0);
                *yank_buf = text[prev..*cursor].to_string();
                text.drain(prev..*cursor);
                *cursor = prev;
                return true;
            }
            false
        }
        "D" => {
            let end = text[*cursor..].find('\n').map(|p| *cursor + p).unwrap_or(text.len());
            if end > *cursor {
                *yank_buf = text[*cursor..end].to_string();
                text.drain(*cursor..end);
                return true;
            }
            false
        }
        "C" => {
            let end = text[*cursor..].find('\n').map(|p| *cursor + p).unwrap_or(text.len());
            *yank_buf = text[*cursor..end].to_string();
            text.drain(*cursor..end);
            *mode = VimMode::Insert;
            true
        }
        "s" => {
            if *cursor < text.len() {
                let clen = text[*cursor..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
                *yank_buf = text[*cursor..*cursor + clen].to_string();
                text.drain(*cursor..*cursor + clen);
                *mode = VimMode::Insert;
                return true;
            }
            false
        }
        "S" => {
            let ls = text[..*cursor].rfind('\n').map(|p| p + 1).unwrap_or(0);
            let le = text[*cursor..].find('\n').map(|p| *cursor + p).unwrap_or(text.len());
            *yank_buf = text[ls..le].to_string();
            text.drain(ls..le);
            *cursor = ls;
            *mode = VimMode::Insert;
            true
        }
        // ---- Yank shortcuts ----
        "Y" | "yy" => {
            let ls = text[..*cursor].rfind('\n').map(|p| p + 1).unwrap_or(0);
            let le = text[*cursor..].find('\n').map(|p| *cursor + p + 1).unwrap_or(text.len());
            *yank_buf = text[ls..le].to_string();
            false
        }
        // ---- Paste ----
        "p" => {
            if !yank_buf.is_empty() {
                let buf = yank_buf.clone();
                let insert_pos = if *cursor < text.len() {
                    text[*cursor..].char_indices().nth(1).map(|(b, _)| *cursor + b).unwrap_or(text.len())
                } else { text.len() };
                text.insert_str(insert_pos, &buf);
                *cursor = (insert_pos + buf.len()).saturating_sub(1);
                return true;
            }
            false
        }
        "P" => {
            if !yank_buf.is_empty() {
                let buf = yank_buf.clone();
                text.insert_str(*cursor, &buf);
                *cursor = (*cursor + buf.len()).saturating_sub(1);
                return true;
            }
            false
        }
        // ---- Replace ----
        "r" => { *pending = VimPendingState::Replace { count: n }; false }
        // ---- Toggle case ----
        "~" => {
            if *cursor < text.len() {
                let clen = text[*cursor..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
                let old: String = text[*cursor..*cursor + clen].to_string();
                let new: String = old.chars().map(|c| {
                    if c.is_uppercase() { c.to_lowercase().next().unwrap_or(c) }
                    else { c.to_uppercase().next().unwrap_or(c) }
                }).collect();
                text.replace_range(*cursor..*cursor + clen, &new);
                if *cursor < text.len() {
                    *cursor = text[*cursor..].char_indices().nth(1).map(|(b, _)| *cursor + b).unwrap_or(text.len());
                }
                return true;
            }
            false
        }
        // ---- Indent ----
        ">" => { *pending = VimPendingState::Indent { dir: '>', count: n }; false }
        "<" => { *pending = VimPendingState::Indent { dir: '<', count: n }; false }
        // ---- Join lines ----
        "J" => {
            if let Some(nl_pos) = text[*cursor..].find('\n').map(|p| *cursor + p) {
                text.remove(nl_pos);
                if text.as_bytes().get(nl_pos) != Some(&b' ') {
                    text.insert(nl_pos, ' ');
                }
                return true;
            }
            false
        }
        // ---- Open line ----
        "o" => {
            let le = text[*cursor..].find('\n').map(|p| *cursor + p).unwrap_or(text.len());
            text.insert(le, '\n');
            *cursor = le + 1;
            *mode = VimMode::Insert;
            true
        }
        "O" => {
            let ls = text[..*cursor].rfind('\n').map(|p| p + 1).unwrap_or(0);
            text.insert(ls, '\n');
            *cursor = ls;
            *mode = VimMode::Insert;
            true
        }
        // ---- dd/yy (multi-char fallthrough from legacy apply_vim_command) ----
        "dd" => {
            let ls = text[..*cursor].rfind('\n').map(|p| p + 1).unwrap_or(0);
            let le = text[*cursor..].find('\n').map(|p| *cursor + p + 1).unwrap_or(text.len());
            *yank_buf = text[ls..le].to_string();
            text.drain(ls..le);
            *cursor = ls.min(text.len());
            true
        }
        // ---- Register, marks, macros — set pending; actual work done in vim_command ----
        "\"" => { *pending = VimPendingState::Register('\0'); false }
        "m" => { *pending = VimPendingState::Mark; false }
        "'" => { *pending = VimPendingState::JumpMark; false }
        "q" => { *pending = VimPendingState::MacroRecord; false }
        "@" => { *pending = VimPendingState::MacroReplay; false }
        _ => false,
    }
}

fn vim_g(
    text: &mut str,
    cursor: &mut usize,
    key: &str,
    pending: &mut VimPendingState,
    count: usize,
) -> bool {
    match key {
        "g" => { *cursor = if count > 1 { motion_gg(text, count) } else { 0 }; false }
        "e" => {
            // `ge` — end of previous word
            for _ in 0..count.max(1) {
                if *cursor == 0 { break; }
                let before = &text[..*cursor];
                let chars: Vec<char> = before.chars().collect();
                let n = chars.len();
                let mut i = n;
                while i > 0 && chars[i - 1].is_whitespace() { i -= 1; }
                if i == 0 { *cursor = 0; break; }
                let is_wc = is_word_char(chars[i - 1]);
                while i > 1 && is_word_char(chars[i - 2]) == is_wc && !chars[i - 2].is_whitespace() { i -= 1; }
                *cursor = char_idx_to_byte(before, i - 1);
            }
            false
        }
        "E" => {
            // `gE` — end of previous WORD
            for _ in 0..count.max(1) {
                if *cursor == 0 { break; }
                let before = &text[..*cursor];
                let chars: Vec<char> = before.chars().collect();
                let n = chars.len();
                let mut i = n;
                while i > 0 && chars[i - 1].is_whitespace() { i -= 1; }
                while i > 1 && !chars[i - 2].is_whitespace() { i -= 1; }
                *cursor = char_idx_to_byte(before, i - 1);
            }
            false
        }
        "U" => {
            // `gU` — start case conversion uppercase operator
            *pending = VimPendingState::Operator { op: VimOperator::Uppercase, count };
            false
        }
        "u" => {
            // `gu` — start case conversion lowercase operator
            *pending = VimPendingState::Operator { op: VimOperator::Lowercase, count };
            false
        }
        _ => { *pending = VimPendingState::None; false }
    }
}

#[allow(clippy::too_many_arguments)]
fn vim_operator(
    mode: &mut VimMode,
    text: &mut String,
    cursor: &mut usize,
    key: &str,
    yank_buf: &mut String,
    pending: &mut VimPendingState,
    _last_find: &mut Option<(VimFindKind, char)>,
    op: VimOperator,
    count: usize,
) -> bool {
    let op_char = match op {
        VimOperator::Delete => "d",
        VimOperator::Change => "c",
        VimOperator::Yank => "y",
        VimOperator::Uppercase => "U",
        VimOperator::Lowercase => "u",
    };
    // Doubled operator = line op (dd, cc, yy, gUU, guu)
    if key == op_char {
        let ls = text[..*cursor].rfind('\n').map(|p| p + 1).unwrap_or(0);
        let mut le = *cursor;
        for _ in 0..count.max(1) {
            match text[le..].find('\n') {
                Some(n) => le += n + 1,
                None => { le = text.len(); break; }
            }
        }
        let le = le.min(text.len());
        let selected = &text[ls..le];
        *yank_buf = selected.to_string();
        if op != VimOperator::Yank {
            let new_content = match op {
                VimOperator::Delete => String::new(),
                VimOperator::Change => {
                    *mode = VimMode::Insert;
                    String::new()
                }
                VimOperator::Uppercase => uppercase_region(selected),
                VimOperator::Lowercase => lowercase_region(selected),
                VimOperator::Yank => unreachable!(),
            };
            text.drain(ls..le);
            text.insert_str(ls, &new_content);
            *cursor = ls;
            return true;
        }
        return false;
    }
    // Count prefix after operator (e.g. d3w)
    if key.len() == 1 && key.chars().next().unwrap().is_ascii_digit() {
        *pending = VimPendingState::OperatorCount { op, count, digits: key.to_string() };
        return false;
    }
    // `g` prefix
    if key == "g" { *pending = VimPendingState::OperatorG { op, count }; return false; }
    // Simple motions
    let target = match key {
        "h" => { let mut p = *cursor; for _ in 0..count.max(1) { p = p.saturating_sub(1); } p }
        "l" => { let mut p = *cursor; for _ in 0..count.max(1) { if p < text.len() { p = text[p..].char_indices().nth(1).map(|(b,_)| p+b).unwrap_or(text.len()); } } p }
        "w" => { let mut p = *cursor; for _ in 0..count.max(1) { p = motion_w(text, p); } p }
        "b" => { let mut p = *cursor; for _ in 0..count.max(1) { p = motion_b(text, p); } p }
        "e" => { let mut p = *cursor; for _ in 0..count.max(1) { p = motion_e(text, p); } p }
        "W" => { let mut p = *cursor; for _ in 0..count.max(1) { p = motion_W(text, p); } p }
        "B" => { let mut p = *cursor; for _ in 0..count.max(1) { p = motion_B(text, p); } p }
        "E" => { let mut p = *cursor; for _ in 0..count.max(1) { p = motion_E(text, p); } p }
        "0" => text[..*cursor].rfind('\n').map(|p| p+1).unwrap_or(0),
        "^" => motion_first_nonblank(text, *cursor),
        "$" => text[*cursor..].find('\n').map(|p| *cursor+p).unwrap_or(text.len()),
        "G" => if count == 1 { motion_G(text) } else { motion_gg(text, count) },
        _ => { return false; }
    };
    if target == *cursor { return false; }
    let (from, to) = if target < *cursor { (target, *cursor) } else { (*cursor, target) };
    // Inclusive adjustment for e, E, $
    let to_adj = if matches!(key, "e" | "E" | "$") {
        text[to..].char_indices().nth(1).map(|(b,_)| to+b).unwrap_or(text.len())
    } else { to };
    let (new_text, new_cursor) = apply_operator_range(op, text, from, to_adj, yank_buf, mode);
    *text = new_text;
    *cursor = new_cursor.min(text.len());
    op != VimOperator::Yank
}

#[allow(clippy::too_many_arguments)]
fn vim_operator_count(
    mode: &mut VimMode,
    text: &mut String,
    cursor: &mut usize,
    key: &str,
    yank_buf: &mut String,
    pending: &mut VimPendingState,
    last_find: &mut Option<(VimFindKind, char)>,
    op: VimOperator,
    count: usize,
    digits: String,
) -> bool {
    if key.len() == 1 && key.chars().next().unwrap().is_ascii_digit() {
        let new_digits = format!("{}{}", digits, key);
        let d: usize = new_digits.parse().unwrap_or(10000).min(10000);
        *pending = VimPendingState::OperatorCount { op, count, digits: d.to_string() };
        return false;
    }
    let motion_count: usize = digits.parse().unwrap_or(1);
    let effective = count.saturating_mul(motion_count).min(10000);
    *pending = VimPendingState::Operator { op, count: effective };
    vim_operator(mode, text, cursor, key, yank_buf, pending, last_find, op, effective)
}

fn vim_operator_g(
    mode: &mut VimMode,
    text: &mut String,
    cursor: &mut usize,
    key: &str,
    yank_buf: &mut String,
    op: VimOperator,
    count: usize,
) -> bool {
    match key {
        "g" => {
            let target = if count > 1 { motion_gg(text, count) } else { 0 };
            let (from, to) = (target.min(*cursor), target.max(*cursor));
            let to_le = text[to..].find('\n').map(|p| to+p+1).unwrap_or(text.len());
            let (new_text, new_cursor) = apply_operator_range(op, text, from, to_le, yank_buf, mode);
            *text = new_text;
            *cursor = new_cursor.min(text.len());
            op != VimOperator::Yank
        }
        _ => false,
    }
}

/// Apply a vim normal-mode motion/command to `text`/`cursor`.
/// Returns the new (text, cursor_pos) after the command.
/// Covers: h j k l w b e 0 $ i a I A dd yy x p
pub fn apply_vim_command(
    mode: &mut VimMode,
    text: &mut String,
    cursor: &mut usize,
    key: &str,
    yank_buf: &mut String,
) {
    match key {
        // Mode transitions
        "i" if *mode == VimMode::Normal => { *mode = VimMode::Insert; }
        "a" if *mode == VimMode::Normal => {
            *mode = VimMode::Insert;
            if *cursor < text.len() { *cursor += 1; }
        }
        "I" if *mode == VimMode::Normal => {
            *mode = VimMode::Insert;
            *cursor = 0;
        }
        "A" if *mode == VimMode::Normal => {
            *mode = VimMode::Insert;
            *cursor = text.len();
        }
        "Escape" => { *mode = VimMode::Normal; }
        // Normal mode motions
        "h" if *mode == VimMode::Normal => {
            *cursor = cursor.saturating_sub(1);
        }
        "l" if *mode == VimMode::Normal => {
            if *cursor < text.len() { *cursor += 1; }
        }
        "0" if *mode == VimMode::Normal => { *cursor = 0; }
        "$" if *mode == VimMode::Normal => { *cursor = text.len(); }
        "w" if *mode == VimMode::Normal => {
            // Move to start of next word
            let rest = &text[*cursor..];
            let skip_word = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').count();
            let skip_space = rest[skip_word..].chars().take_while(|c| c.is_whitespace()).count();
            *cursor = (*cursor + skip_word + skip_space).min(text.len());
        }
        "b" if *mode == VimMode::Normal => {
            // Move to start of previous word
            let before = &text[..*cursor];
            let skip_space = before.chars().rev().take_while(|c| c.is_whitespace()).count();
            let skip_word = before[..before.len() - skip_space].chars().rev().take_while(|c| c.is_alphanumeric() || *c == '_').count();
            *cursor = cursor.saturating_sub(skip_space + skip_word);
        }
        "x" if *mode == VimMode::Normal => {
            // Delete char under cursor
            if *cursor < text.len() {
                *yank_buf = text.chars().nth(*cursor).unwrap_or_default().to_string();
                text.remove(*cursor);
                if *cursor > 0 && *cursor >= text.len() { *cursor = text.len().saturating_sub(1); }
            }
        }
        "dd" if *mode == VimMode::Normal => {
            // Delete current line
            *yank_buf = text.clone();
            text.clear();
            *cursor = 0;
        }
        "yy" if *mode == VimMode::Normal => {
            *yank_buf = text.clone();
        }
        "p" if *mode == VimMode::Normal => {
            // Paste after cursor
            let insert_pos = (*cursor + 1).min(text.len());
            text.insert_str(insert_pos, yank_buf);
            *cursor = insert_pos + yank_buf.len();
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Typeahead / autocomplete
// ---------------------------------------------------------------------------

/// Typeahead source.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeaheadSource {
    SlashCommand,
    FileRef,
    History,
}

/// A single typeahead suggestion.
#[derive(Debug, Clone)]
pub struct TypeaheadSuggestion {
    pub text: String,
    pub description: String,
    pub source: TypeaheadSource,
}

/// Compute typeahead suggestions for the current input.
///
/// Handles two kinds of suggestions:
/// - `/` slash commands (e.g. `/help`, `/clear`)
/// - `@` file references (e.g. `@src/`, `@~/Documents/`)
pub fn compute_typeahead(
    input: &str,
    slash_commands: &[(&str, &str)],
    file_autocomplete_limit: usize,
    file_autocomplete_show_hidden: bool,
) -> Vec<TypeaheadSuggestion> {
    // Handle slash commands: /help, /clear, etc.
    if input.starts_with('/') {
        return compute_slash_suggestions(input, slash_commands);
    }

    // Handle file references: @, @/, @~/, @src/, etc.
    compute_file_suggestions(input, file_autocomplete_limit, file_autocomplete_show_hidden)
}

/// Compute typeahead suggestions for slash commands only (e.g., `/help`).
pub(crate) fn compute_slash_suggestions(input: &str, slash_commands: &[(&str, &str)]) -> Vec<TypeaheadSuggestion> {
    let mut suggestions = Vec::new();

    if let Some(cmd_prefix) = input.strip_prefix('/') {
        let prefix_lower = cmd_prefix.to_lowercase();
        for (name, desc) in slash_commands {
            if name.to_lowercase().starts_with(&prefix_lower) {
                suggestions.push(TypeaheadSuggestion {
                    text: format!("/{}", name),
                    description: desc.to_string(),
                    source: TypeaheadSource::SlashCommand,
                });
            }
        }
    }

    suggestions
}

/// Compute typeahead suggestions for file references (e.g., `@src/main.rs`).
pub(crate) fn compute_file_suggestions(
    input: &str,
    file_autocomplete_limit: usize,
    file_autocomplete_show_hidden: bool,
) -> Vec<TypeaheadSuggestion> {
    let mut suggestions = Vec::new();

    if let Some(at_idx) = input.rfind('@') {
        // Only suggest files if @ is at a word boundary (preceded by whitespace or start of string)
        let at_word_boundary = at_idx == 0
            || input[..at_idx]
                .chars()
                .last()
                .map(|c| c.is_whitespace())
                .unwrap_or(false);

        if at_word_boundary {
            let file_prefix = &input[at_idx + 1..];
            suggestions = suggest_files(file_prefix, file_autocomplete_limit, file_autocomplete_show_hidden);
        }
    }

    suggestions
}

/// Suggest files matching a path prefix.
///
/// Examples:
/// - `""` → files in cwd with names only (e.g., ["main.rs", "lib.rs"])
/// - `"src"` → suggest "src/" if it exists
/// - `"src/"` → files in src/ with names only (e.g., ["main.rs", "lib.rs"])
/// - `"/"` → files in root with full paths (e.g., ["/Users", "/Applications"])
/// - `"~"` → suggest "~/" if it exists
/// - `"~/"` → files in home with names only
///
/// Note: calls `fs::read_dir` synchronously on every invocation; may stall on slow/network
/// filesystems. Consider debouncing at the call site if this becomes a problem.
fn suggest_files(prefix: &str, max_suggestions: usize, show_hidden: bool) -> Vec<TypeaheadSuggestion> {
    use std::path::PathBuf;
    use std::fs;

    let mut suggestions = Vec::new();

    // Determine the directory to list and whether to show full paths
    let (search_dir, show_full_paths, partial_name) = if prefix.is_empty() {
        // Just @, show files from cwd
        if let Ok(cwd) = std::env::current_dir() {
            (cwd, false, String::new())
        } else {
            return suggestions;
        }
    } else if prefix.starts_with('/') || prefix.starts_with('~') {
        // Absolute or home path: show full paths
        let expanded = if prefix.starts_with('~') {
            prefix.replacen('~', &home_dir().unwrap_or_default(), 1)
        } else {
            prefix.to_string()
        };

        let path = PathBuf::from(&expanded);
        if path.is_dir() && prefix.ends_with('/') {
            // User typed a complete directory with trailing slash: list its contents
            (path, true, String::new())
        } else if let Some(parent) = path.parent() {
            // User typed a partial path or directory without slash: list parent's contents and filter
            let partial = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_string();
            (parent.to_path_buf(), true, partial)
        } else {
            return suggestions;
        }
    } else {
        // Relative path in cwd
        if let Ok(cwd) = std::env::current_dir() {
            let path = cwd.join(prefix);
            if path.is_dir() && prefix.ends_with('/') {
                // Complete directory with trailing slash: list its contents
                (path, false, String::new())
            } else if let Some(parent) = path.parent() {
                // Partial path or directory without slash: list parent and filter
                let partial = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
                (parent.to_path_buf(), false, partial)
            } else {
                return suggestions;
            }
        } else {
            return suggestions;
        }
    };

    // List files in the directory
    if let Ok(entries) = fs::read_dir(&search_dir) {
        let mut files: Vec<_> = entries
            .filter_map(|e| {
                e.ok().and_then(|entry| {
                    let path = entry.path();
                    let name = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|s| s.to_string())?;

                    // Filter by partial name (case-insensitive)
                    if !partial_name.is_empty() && !name.to_lowercase().starts_with(&partial_name.to_lowercase()) {
                        return None;
                    }

                    // Filter hidden files unless user explicitly types a dot or show_hidden_files is enabled
                    if !show_hidden && name.starts_with('.') && !partial_name.to_lowercase().starts_with('.') {
                        return None;
                    }

                    // Detect if this is a symlink or junction link
                    let is_symlink = entry.file_type().ok().map(|ft| ft.is_symlink()).unwrap_or(false);
                    let is_dir = path.is_dir();

                    Some((name, is_dir, is_symlink, path))
                })
            })
            .collect();

        files.sort_by(|a, b| {
            // Directories first, then alphabetically
            match (b.1, a.1) {
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                _ => a.0.cmp(&b.0),
            }
        });

        for (name, is_dir, is_symlink, full_path) in files {
            if suggestions.len() >= max_suggestions {
                break;
            }

            if is_dir && !dir_has_visible_contents(&full_path, show_hidden) {
                continue;
            }

            let is_listing_mode = prefix.ends_with('/');
            let suggestion_text = if show_full_paths {
                let full = search_dir.join(&name);
                full.to_string_lossy().to_string()
                    + if is_dir { "/" } else { "" }
            } else if is_listing_mode {
                // When listing a directory's contents, prepend the full prefix path
                format!("{}{}{}", prefix, name, if is_dir { "/" } else { "" })
            } else if !partial_name.is_empty() && prefix.ends_with(&partial_name) {
                // When filtering in a subdirectory, prepend the parent path
                let parent_path = &prefix[..prefix.len() - partial_name.len()];
                format!("{}{}{}", parent_path, name, if is_dir { "/" } else { "" })
            } else {
                // Fallback: just use the matched filename
                name.clone() + if is_dir { "/" } else { "" }
            };

            let description = if is_symlink {
                if is_dir {
                    "directory link".to_string()
                } else {
                    "file link".to_string()
                }
            } else if is_dir {
                "directory".to_string()
            } else {
                "file".to_string()
            };

            suggestions.push(TypeaheadSuggestion {
                text: format!("@{}", suggestion_text),
                description,
                source: TypeaheadSource::FileRef,
            });
        }
    }

    suggestions
}

/// Returns true if `dir` contains at least one visible entry.
/// When `show_hidden` is false, dotfiles are not counted as visible.
fn dir_has_visible_contents(dir: &std::path::Path, show_hidden: bool) -> bool {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok()).any(|entry| {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            show_hidden || !name_str.starts_with('.')
        }),
        Err(_) => false,
    }
}

/// Get the home directory path.
fn home_dir() -> Option<String> {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
}

// ---------------------------------------------------------------------------
// Paste handling
// ---------------------------------------------------------------------------

/// Handle a paste event.
///
/// Large pastes (≥3 lines or >150 chars) are replaced with a compact
/// placeholder like `[Pasted text #3 +12 lines]` (the shared claurst-core
/// reference format) while the real content is stored in `paste_contents`.
/// The placeholder is expanded back into the full content at submit time
/// (`take()`), by clicking it, or via the `expandPaste` keybinding.
/// Single-line short strings are inserted verbatim.
/// Normalize `\r\n` and lone `\r` into `\n`.
///
/// The prompt buffer must never contain a bare carriage return: the renderer
/// splits logical lines on `\n` and advances the cursor's byte offset assuming
/// a single-byte separator, so a `\r` (or CRLF pair) desyncs that accounting
/// and can slice mid-codepoint (#221).
fn normalize_newlines(s: &str) -> String {
    if s.contains('\r') {
        s.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        s.to_string()
    }
}

pub fn handle_paste(
    content: &str,
    paste_counter: &mut u32,
) -> (String, Option<String>) {
    let line_count = content.lines().count();
    let is_large = line_count >= 3 || content.len() > 150;
    if !is_large {
        return (content.to_string(), None);
    }
    *paste_counter += 1;
    let num_lines = claurst_core::prompt_history::get_pasted_text_ref_num_lines(content);
    let placeholder =
        claurst_core::prompt_history::format_pasted_text_ref(*paste_counter, num_lines);
    (placeholder, Some(content.to_string()))
}

/// Normalize a pasted string into a filesystem path if it looks like one.
///
/// Handles:
/// - `file:///path/to/file` — URL-encoded paths
/// - `"C:\path"` / `'/path'` — quoted paths (strips quotes)
/// - Bare absolute paths (`/home/...`, `C:\...`)
///
/// Returns `None` if the text is multiline, not path-shaped, or the resolved
/// path does not exist on the filesystem.  Callers can use the returned
/// `PathBuf` to decide whether to treat the paste as a file attachment.
pub fn detect_pasted_path(text: &str) -> Option<std::path::PathBuf> {
    let trimmed = text.trim();
    // Multiline content is never a bare path.
    if trimmed.contains('\n') {
        return None;
    }
    // Strip outer matching quotes.
    let unquoted = trimmed
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| trimmed.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(trimmed);

    // file:// URL — strip the scheme (skip the leading //).
    let candidate = if let Some(rest) = unquoted.strip_prefix("file://") {
        rest
    } else {
        unquoted
    };

    let path = std::path::Path::new(candidate);
    if path.is_absolute() && path.exists() {
        Some(path.to_path_buf())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Kill ring (Emacs-style kill/yank system)
// ---------------------------------------------------------------------------

/// Kill ring stores accumulated kills (deleted text) for cycling through with Alt+Y.
/// Maintains a FIFO list of kills with a current index for cycling backward.
#[derive(Debug, Clone)]
pub struct KillRing {
    /// List of killed text entries. Most recent is last.
    pub entries: Vec<String>,
    /// Maximum number of entries to keep (prevents unbounded growth).
    max_size: usize,
    /// Current position in kill ring when cycling with Alt+Y (None = most recent).
    pub current_index: Option<usize>,
    /// Tracks whether the last action was a kill (for combining consecutive kills).
    pub last_was_kill: bool,
}

impl KillRing {
    /// Create a new kill ring with default capacity.
    pub fn new() -> Self {
        Self {
            entries: Vec::with_capacity(32),
            max_size: 64,
            current_index: None,
            last_was_kill: false,
        }
    }

    /// Add a kill entry. If the last operation was a kill, append to the most recent entry
    /// instead of creating a new one (for combining consecutive kills).
    pub fn push(&mut self, text: String) {
        if text.is_empty() {
            return;
        }

        if self.last_was_kill && !self.entries.is_empty() {
            // Append to the most recent entry (last_was_kill combines consecutive kills)
            self.entries.last_mut().unwrap().push_str(&text);
        } else {
            // New kill entry
            self.entries.push(text);
            if self.entries.len() > self.max_size {
                self.entries.remove(0);
            }
        }
        self.current_index = None; // Reset cycling to most recent
        self.last_was_kill = true;
    }

    /// Get the current kill to paste (most recent or current index if cycling).
    pub fn get_current(&self) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }

        match self.current_index {
            None => self.entries.last().map(|s| s.as_str()),
            Some(idx) => self.entries.get(idx).map(|s| s.as_str()),
        }
    }

    /// Cycle backward through kill ring (Alt+Y after paste).
    pub fn cycle_backward(&mut self) {
        if self.entries.is_empty() {
            return;
        }

        match self.current_index {
            None => {
                // Start cycling from the second-to-last entry
                if self.entries.len() > 1 {
                    self.current_index = Some(self.entries.len() - 2);
                }
            }
            Some(0) => {
                // Wrap around to the end
                self.current_index = Some(self.entries.len() - 1);
            }
            Some(idx) => {
                self.current_index = Some(idx - 1);
            }
        }
    }

    /// Mark that a non-kill action occurred (resets consecutive kill combination).
    pub fn mark_non_kill(&mut self) {
        self.last_was_kill = false;
    }
}

impl Default for KillRing {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// PromptInput state
// ---------------------------------------------------------------------------

/// Input mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InputMode {
    #[default]
    Default,
    Plan,
    Readonly,
}

/// Full state for the prompt input widget.
#[derive(Debug, Clone)]
pub struct PromptInputState {
    /// Current text content.
    pub text: String,
    /// Cursor position (byte offset into `text`).
    pub cursor: usize,
    /// Current vim mode.
    pub vim_mode: VimMode,
    /// Whether vim mode is enabled.
    pub vim_enabled: bool,
    /// Input mode (default / plan / readonly).
    pub mode: InputMode,
    /// Typeahead suggestions.
    pub suggestions: Vec<TypeaheadSuggestion>,
    /// Currently selected suggestion index.
    pub suggestion_index: Option<usize>,
    /// History entries for ↑↓ navigation.
    pub history: Vec<String>,
    /// Current history position (-1 = not browsing history).
    pub history_pos: Option<usize>,
    /// Saved draft while browsing history.
    pub history_draft: String,
    /// Paste counter for placeholder numbering.
    pub paste_counter: u32,
    /// Stored paste contents: counter → content.
    pub paste_contents: std::collections::HashMap<u32, String>,
    /// Yank buffer for vim operations.
    pub yank_buf: String,
    /// Estimated token count for current text.
    pub token_estimate: usize,
    /// Pending multi-key vim command state (persists across keystrokes).
    pub vim_pending: VimPendingState,
    /// Undo stack: Vec of (text, cursor) snapshots before modifications.
    pub undo_stack: Vec<(String, usize)>,
    /// Visual mode selection anchor (byte offset).
    pub visual_anchor: Option<usize>,
    /// Last f/F/t/T find for `;`/`,` repeat.
    pub last_find: Option<(VimFindKind, char)>,
    /// Named registers: key is the register name char (a-z, 0-9, etc.), value is text.
    pub vim_registers: std::collections::HashMap<char, String>,
    /// Macro recording state: Some(register_name) when recording.
    pub vim_macro_recording: Option<char>,
    /// Recorded macro content (accumulates key descriptions while recording).
    pub vim_macro_content: std::collections::HashMap<char, Vec<String>>,
    /// Named marks: maps mark char to (text, cursor) snapshots.
    pub vim_marks: std::collections::HashMap<char, (String, usize)>,
    /// The last modifying command for dot-repeat.
    pub vim_dot_action: Option<DotRepeatAction>,
    /// Pending insert-mode text (accumulates between entering and leaving insert mode).
    vim_insert_text_before: Option<String>,
    /// Command-line buffer for `:` command mode.
    pub vim_command_buf: String,
    /// In-prompt search buffer for `/` search mode.
    pub vim_search_buf: String,
    /// Last executed search pattern for `n`/`N` navigation.
    pub vim_search_last: Option<String>,
    /// Set by `:q`/`:wq` — the app loop should check and honour this.
    pub vim_quit_requested: bool,
    /// Pending image attachments (from clipboard paste) to be sent with next message.
    pub pending_images: Vec<crate::image_paste::PastedImage>,
    /// Emacs-style kill ring for Ctrl+K, Ctrl+U, Ctrl+W operations.
    pub kill_ring: KillRing,
}

impl PromptInputState {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            vim_mode: VimMode::Insert,
            vim_enabled: false,
            mode: InputMode::Default,
            suggestions: Vec::new(),
            suggestion_index: None,
            history: Vec::new(),
            history_pos: None,
            history_draft: String::new(),
            paste_counter: 0,
            paste_contents: std::collections::HashMap::new(),
            yank_buf: String::new(),
            token_estimate: 0,
            vim_pending: VimPendingState::None,
            undo_stack: Vec::new(),
            visual_anchor: None,
            last_find: None,
            vim_registers: std::collections::HashMap::new(),
            vim_macro_recording: None,
            vim_macro_content: std::collections::HashMap::new(),
            vim_marks: std::collections::HashMap::new(),
            vim_dot_action: None,
            vim_insert_text_before: None,
            vim_command_buf: String::new(),
            vim_search_buf: String::new(),
            vim_search_last: None,
            vim_quit_requested: false,
            pending_images: Vec::new(),
            kill_ring: KillRing::new(),
        }
    }

    /// Add a clipboard image attachment to the pending list.
    pub fn add_image(&mut self, img: crate::image_paste::PastedImage) {
        self.pending_images.push(img);
    }

    /// Drain and return all pending image attachments (called at send time).
    pub fn clear_images(&mut self) -> Vec<crate::image_paste::PastedImage> {
        std::mem::take(&mut self.pending_images)
    }

    /// Insert a character at cursor position.
    pub fn insert_char(&mut self, c: char) {
        if self.mode == InputMode::Readonly { return; }
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        self.update_token_estimate();
    }

    /// Insert a newline (Shift+Enter).
    pub fn insert_newline(&mut self) {
        if self.mode == InputMode::Readonly { return; }
        self.insert_char('\n');
    }

    /// Delete the character before cursor.
    pub fn backspace(&mut self) {
        if self.cursor == 0 || self.mode == InputMode::Readonly { return; }
        let prev = self.text[..self.cursor]
            .char_indices()
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.text.remove(prev);
        self.cursor = prev;
        self.update_token_estimate();
    }

    /// Delete the character at cursor.
    pub fn delete(&mut self) {
        if self.cursor >= self.text.len() || self.mode == InputMode::Readonly { return; }
        self.text.remove(self.cursor);
        self.update_token_estimate();
    }

    /// Move cursor left.
    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            let prev = self.text[..self.cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.cursor = prev;
        }
    }

    /// Move cursor right.
    pub fn move_right(&mut self) {
        if self.cursor < self.text.len() {
            let next = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
            self.cursor = next;
        }
    }

    /// Navigate history up (older).
    pub fn history_up(&mut self) {
        if self.history.is_empty() { return; }
        match self.history_pos {
            None => {
                self.history_draft = self.text.clone();
                self.history_pos = Some(self.history.len() - 1);
            }
            Some(0) => {}
            Some(n) => {
                self.history_pos = Some(n - 1);
            }
        }
        if let Some(pos) = self.history_pos {
            self.text = self.history[pos].clone();
            self.cursor = self.text.len();
            self.update_token_estimate();
        }
    }

    /// Navigate history down (newer).
    pub fn history_down(&mut self) {
        match self.history_pos {
            None => {}
            Some(n) if n + 1 >= self.history.len() => {
                self.history_pos = None;
                self.text = self.history_draft.clone();
                self.cursor = self.text.len();
                self.update_token_estimate();
            }
            Some(n) => {
                self.history_pos = Some(n + 1);
                self.text = self.history[n + 1].clone();
                self.cursor = self.text.len();
                self.update_token_estimate();
            }
        }
    }

    /// Handle a paste event.
    pub fn paste(&mut self, content: &str) {
        // Normalize CRLF / lone CR into LF before storing or inserting. A stray
        // '\r' in the buffer desyncs the render cursor's byte accounting (which
        // assumes a 1-byte '\n' separator) and can slice mid-codepoint (#221).
        // Stored paste bodies are normalized too, because expanding a
        // placeholder splices the body back into the buffer.
        let content = normalize_newlines(content);
        let (text, stored) = handle_paste(&content, &mut self.paste_counter);
        if let Some(stored_content) = stored {
            self.paste_contents.insert(self.paste_counter, stored_content);
        }
        for c in text.chars() {
            self.text.insert(self.cursor, c);
            self.cursor += c.len_utf8();
        }
        self.update_token_estimate();
        self.kill_ring.mark_non_kill();
    }

    /// Ctrl+K: Cut from cursor to end of line and save to kill ring.
    pub fn kill_line(&mut self) {
        if self.mode == InputMode::Readonly { return; }
        let line_end = self.text[self.cursor..].find('\n')
            .map(|p| self.cursor + p)
            .unwrap_or(self.text.len());

        if line_end > self.cursor {
            let killed = self.text.drain(self.cursor..line_end).collect::<String>();
            self.kill_ring.push(killed);
            self.update_token_estimate();
        }
    }

    /// Ctrl+U: Cut from line start to cursor and save to kill ring.
    pub fn kill_line_backward(&mut self) {
        if self.mode == InputMode::Readonly { return; }
        let line_start = self.text[..self.cursor].rfind('\n')
            .map(|p| p + 1)
            .unwrap_or(0);

        if self.cursor > line_start {
            let killed = self.text.drain(line_start..self.cursor).collect::<String>();
            self.kill_ring.push(killed);
            self.cursor = line_start;
            self.update_token_estimate();
        }
    }

    /// Ctrl+W: Cut previous word and save to kill ring.
    pub fn kill_word_backward(&mut self) {
        if self.mode == InputMode::Readonly || self.cursor == 0 { return; }
        let before = &self.text[..self.cursor];
        let chars: Vec<char> = before.chars().collect();
        let mut idx = chars.len();
        while idx > 0 && chars[idx - 1].is_whitespace() {
            idx -= 1;
        }
        if idx == 0 {
            return;
        }
        if is_word_char(chars[idx - 1]) {
            while idx > 0 && is_word_char(chars[idx - 1]) {
                idx -= 1;
            }
        } else {
            while idx > 0 && !is_word_char(chars[idx - 1]) && !chars[idx - 1].is_whitespace() {
                idx -= 1;
            }
        }
        let kill_start = char_idx_to_byte(before, idx);
        if kill_start < self.cursor {
            let killed = self.text.drain(kill_start..self.cursor).collect::<String>();
            self.kill_ring.push(killed);
            self.cursor = kill_start;
            self.update_token_estimate();
        }
    }

    /// Ctrl+Y: Paste from kill ring (most recent).
    pub fn yank(&mut self) {
        if self.mode == InputMode::Readonly { return; }
        if let Some(text) = self.kill_ring.get_current() {
            for c in text.chars() {
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
            }
            self.update_token_estimate();
            self.kill_ring.mark_non_kill();
        }
    }

    /// Alt+Y: Cycle through kill ring backward.
    pub fn yank_pop(&mut self) {
        if self.mode == InputMode::Readonly { return; }
        self.kill_ring.cycle_backward();
    }

    /// Alt+Backspace: Delete word backward.
    pub fn delete_word_backward(&mut self) {
        if self.mode == InputMode::Readonly || self.cursor == 0 { return; }
        let before = &self.text[..self.cursor];
        let chars: Vec<char> = before.chars().collect();
        let mut idx = chars.len();
        while idx > 0 && chars[idx - 1].is_whitespace() {
            idx -= 1;
        }
        if idx == 0 {
            return;
        }
        if is_word_char(chars[idx - 1]) {
            while idx > 0 && is_word_char(chars[idx - 1]) {
                idx -= 1;
            }
        } else {
            while idx > 0 && !is_word_char(chars[idx - 1]) && !chars[idx - 1].is_whitespace() {
                idx -= 1;
            }
        }
        let delete_start = char_idx_to_byte(before, idx);
        if delete_start < self.cursor {
            self.text.drain(delete_start..self.cursor);
            self.cursor = delete_start;
            self.update_token_estimate();
            self.kill_ring.mark_non_kill();
        }
    }

    /// Alt+Delete: Delete word forward.
    pub fn delete_word_forward(&mut self) {
        if self.mode == InputMode::Readonly || self.cursor >= self.text.len() { return; }
        let rest = &self.text[self.cursor..];
        let chars: Vec<char> = rest.chars().collect();
        let mut idx = 0;
        while idx < chars.len() && chars[idx].is_whitespace() {
            idx += 1;
        }
        if idx >= chars.len() {
            return;
        }
        if is_word_char(chars[idx]) {
            while idx < chars.len() && is_word_char(chars[idx]) {
                idx += 1;
            }
        } else {
            while idx < chars.len() && !is_word_char(chars[idx]) && !chars[idx].is_whitespace() {
                idx += 1;
            }
        }
        let delete_end = self.cursor + char_idx_to_byte(rest, idx);
        if delete_end > self.cursor {
            self.text.drain(self.cursor..delete_end);
            self.update_token_estimate();
            self.kill_ring.mark_non_kill();
        }
    }

    /// Alt+B: Jump to previous word.
    pub fn move_word_backward(&mut self) {
        if self.cursor == 0 { return; }
        let before = &self.text[..self.cursor];
        let chars: Vec<char> = before.chars().collect();
        let mut idx = chars.len();
        while idx > 0 && chars[idx - 1].is_whitespace() {
            idx -= 1;
        }
        if idx == 0 {
            return;
        }
        if is_word_char(chars[idx - 1]) {
            while idx > 0 && is_word_char(chars[idx - 1]) {
                idx -= 1;
            }
        } else {
            while idx > 0 && !is_word_char(chars[idx - 1]) && !chars[idx - 1].is_whitespace() {
                idx -= 1;
            }
        }
        self.cursor = char_idx_to_byte(before, idx);
    }

    /// Alt+F: Jump to next word.
    pub fn move_word_forward(&mut self) {
        if self.cursor >= self.text.len() { return; }
        let rest = &self.text[self.cursor..];
        let chars: Vec<char> = rest.chars().collect();
        let mut idx = 0;
        if idx < chars.len() {
            if is_word_char(chars[idx]) {
                while idx < chars.len() && is_word_char(chars[idx]) {
                    idx += 1;
                }
            } else if !chars[idx].is_whitespace() {
                while idx < chars.len() && !is_word_char(chars[idx]) && !chars[idx].is_whitespace() {
                    idx += 1;
                }
            }
        }
        while idx < chars.len() && chars[idx].is_whitespace() {
            idx += 1;
        }
        self.cursor += char_idx_to_byte(rest, idx);
    }

    /// Alt+D: Delete word after cursor.
    pub fn delete_word_at_cursor(&mut self) {
        if self.mode == InputMode::Readonly || self.cursor >= self.text.len() { return; }
        let rest = &self.text[self.cursor..];
        let chars: Vec<char> = rest.chars().collect();
        let mut idx = 0;
        while idx < chars.len() && chars[idx].is_whitespace() {
            idx += 1;
        }
        if idx >= chars.len() {
            return;
        }
        if is_word_char(chars[idx]) {
            while idx < chars.len() && is_word_char(chars[idx]) {
                idx += 1;
            }
        } else {
            while idx < chars.len() && !is_word_char(chars[idx]) && !chars[idx].is_whitespace() {
                idx += 1;
            }
        }
        let delete_end = self.cursor + char_idx_to_byte(rest, idx);
        if delete_end > self.cursor {
            self.text.drain(self.cursor..delete_end);
            self.update_token_estimate();
            self.kill_ring.mark_non_kill();
        }
    }

    /// Apply a vim command using the full state-machine key handler.
    pub fn vim_command(&mut self, key: &str) {
        // ---- Escape always cancels recording, pending state, returns to Normal ----
        if key == "Escape" {
            // If leaving insert mode, finalise dot-repeat insert action
            if self.vim_mode == VimMode::Insert {
                if let Some(before) = self.vim_insert_text_before.take() {
                    // Compute inserted text as the new characters added since mode entry
                    let inserted = if self.text.len() >= before.len() {
                        // Simple case: text only grew (cursor at end of inserted span)
                        let from = before.len().min(self.cursor);
                        let _ = from; // use cursor-based diff below
                        // Find the diff between before/after texts at current cursor
                        // Inserted = text[insert_start..cursor] but we don't track start.
                        // Approximate: whole text minus before, substring at cursor.
                        // Better: store cursor-at-entry and extract.
                        self.text[before.len().min(self.text.len())..self.cursor.min(self.text.len())].to_string()
                    } else {
                        String::new()
                    };
                    if !inserted.is_empty() {
                        self.vim_dot_action = Some(DotRepeatAction::Insert {
                            text: inserted,
                            mode_after_insert: false,
                        });
                    }
                }
            }
            self.vim_mode = VimMode::Normal;
            self.vim_pending = VimPendingState::None;
            self.visual_anchor = None;
            self.normalize();
            return;
        }

        // ---- Command-line mode (`:`) ----
        if self.vim_mode == VimMode::Command {
            match key {
                "Backspace" => {
                    if self.vim_command_buf.is_empty() {
                        self.vim_mode = VimMode::Normal;
                    } else {
                        self.vim_command_buf.pop();
                    }
                }
                "Enter" => {
                    let cmd = self.vim_command_buf.trim().to_string();
                    self.vim_command_buf.clear();
                    self.vim_mode = VimMode::Normal;
                    self.execute_vim_cmdline(&cmd);
                }
                _ if key.len() == 1 => {
                    self.vim_command_buf.push(key.chars().next().unwrap());
                }
                _ => {}
            }
            return;
        }

        // ---- In-prompt search mode (`/`) ----
        if self.vim_mode == VimMode::Search {
            match key {
                "Backspace" => {
                    if self.vim_search_buf.is_empty() {
                        self.vim_mode = VimMode::Normal;
                    } else {
                        self.vim_search_buf.pop();
                    }
                }
                "Enter" => {
                    let pattern = self.vim_search_buf.clone();
                    if !pattern.is_empty() {
                        self.vim_search_last = Some(pattern.clone());
                        self.vim_search_forward(&pattern, 0);
                    }
                    self.vim_search_buf.clear();
                    self.vim_mode = VimMode::Normal;
                }
                _ if key.len() == 1 => {
                    self.vim_search_buf.push(key.chars().next().unwrap());
                }
                _ => {}
            }
            return;
        }

        // ---- Accumulate key into macro recording buffer ----
        if let Some(reg) = self.vim_macro_recording {
            // `q` in normal mode stops recording
            if key == "q" && self.vim_mode == VimMode::Normal
                && self.vim_pending == VimPendingState::None
            {
                self.stop_macro_recording();
                return;
            }
            if let Some(keys) = self.vim_macro_content.get_mut(&reg) {
                keys.push(key.to_string());
            }
        }

        // ---- Handle new pending states before apply_vim_key ----
        match self.vim_pending.clone() {
            VimPendingState::Register('\0') => {
                // Waiting for register name char after `"`
                if key.len() == 1 {
                    let reg = key.chars().next().unwrap();
                    self.vim_pending = VimPendingState::RegisterOp(reg);
                } else {
                    self.vim_pending = VimPendingState::None;
                }
                return;
            }
            VimPendingState::RegisterOp(reg) => {
                // Waiting for operator after `"<reg>`
                match key {
                    "y" => {
                        // Yank current line to register
                        let ls = self.text[..self.cursor].rfind('\n').map(|p| p + 1).unwrap_or(0);
                        let le = self.text[self.cursor..].find('\n')
                            .map(|p| self.cursor + p + 1)
                            .unwrap_or(self.text.len());
                        let yanked = self.text[ls..le].to_string();
                        self.yank_to_register(reg, &yanked);
                        self.yank_buf = yanked;
                    }
                    "d" => {
                        // Delete current line to register
                        let ls = self.text[..self.cursor].rfind('\n').map(|p| p + 1).unwrap_or(0);
                        let le = self.text[self.cursor..].find('\n')
                            .map(|p| self.cursor + p + 1)
                            .unwrap_or(self.text.len());
                        let deleted = self.text[ls..le].to_string();
                        self.push_undo();
                        self.yank_to_register(reg, &deleted);
                        self.yank_buf = deleted;
                        let le = le.min(self.text.len());
                        self.text.drain(ls..le);
                        self.cursor = ls.min(self.text.len());
                        self.vim_pending = VimPendingState::None;
                        self.normalize();
                        return;
                    }
                    "p" => {
                        // Paste from register after cursor
                        if let Some(buf) = self.paste_from_register(reg) {
                            let insert_pos = if self.cursor < self.text.len() {
                                self.text[self.cursor..].char_indices().nth(1)
                                    .map(|(b, _)| self.cursor + b)
                                    .unwrap_or(self.text.len())
                            } else {
                                self.text.len()
                            };
                            self.push_undo();
                            self.text.insert_str(insert_pos, &buf);
                            self.cursor = (insert_pos + buf.len()).saturating_sub(1);
                            self.vim_pending = VimPendingState::None;
                            self.normalize();
                            return;
                        }
                    }
                    _ => {}
                }
                self.vim_pending = VimPendingState::None;
                return;
            }
            VimPendingState::Mark => {
                // `m<char>` — set mark
                if key.len() == 1 {
                    let name = key.chars().next().unwrap();
                    self.set_mark(name);
                }
                self.vim_pending = VimPendingState::None;
                return;
            }
            VimPendingState::JumpMark => {
                // `'<char>` — jump to mark
                if key.len() == 1 {
                    let name = key.chars().next().unwrap();
                    self.jump_to_mark(name);
                }
                self.vim_pending = VimPendingState::None;
                return;
            }
            VimPendingState::MacroRecord => {
                // `q<char>` — start recording into register; clear pending first.
                self.vim_pending = VimPendingState::None;
                if key.len() == 1 {
                    let reg = key.chars().next().unwrap();
                    self.start_macro_recording(reg);
                }
                return;
            }
            VimPendingState::MacroReplay => {
                // `@<char>` — replay macro; clear pending BEFORE recursing so
                // recursive vim_command calls don't re-enter this arm.
                self.vim_pending = VimPendingState::None;
                if key.len() == 1 {
                    let reg = key.chars().next().unwrap();
                    let keys = self.replay_macro(reg);
                    // Replay each recorded key (avoid infinite loops by cloning)
                    for k in keys {
                        // Guard: don't replay if we somehow entered macro record for same reg
                        if self.vim_macro_recording == Some(reg) { break; }
                        self.vim_command(&k.clone());
                    }
                }
                return;
            }
            _ => {}
        }

        // ---- Dot-repeat `.` — replay last modifying action ----
        if key == "." && self.vim_mode == VimMode::Normal
            && self.vim_pending == VimPendingState::None
        {
            if let Some(action) = self.vim_dot_action.clone() {
                match action {
                    DotRepeatAction::Insert { text: ins, .. } => {
                        self.push_undo();
                        self.text.insert_str(self.cursor, &ins);
                        self.cursor += ins.len();
                        self.normalize();
                        return;
                    }
                    DotRepeatAction::DeleteChars { count } => {
                        self.push_undo();
                        let mut deleted = 0usize;
                        while deleted < count && self.cursor < self.text.len() {
                            let clen = self.text[self.cursor..].chars().next()
                                .map(|c| c.len_utf8()).unwrap_or(1);
                            self.text.drain(self.cursor..self.cursor + clen);
                            deleted += 1;
                        }
                        self.normalize();
                        return;
                    }
                    DotRepeatAction::Change { deleted: _del, inserted: ins } => {
                        self.push_undo();
                        self.text.insert_str(self.cursor, &ins);
                        self.cursor += ins.len();
                        self.normalize();
                        return;
                    }
                    DotRepeatAction::ReplaceChar { ch } => {
                        if self.cursor < self.text.len() {
                            self.push_undo();
                            let clen = self.text[self.cursor..].chars().next()
                                .map(|c| c.len_utf8()).unwrap_or(1);
                            self.text.replace_range(self.cursor..self.cursor + clen, &ch.to_string());
                            self.normalize();
                        }
                        return;
                    }
                }
            }
            return;
        }

        // ---- Track when entering insert mode for dot-repeat ----
        let was_normal = self.vim_mode == VimMode::Normal;
        let prev_text_len = self.text.len();

        // `u` — undo: restore previous text/cursor snapshot
        if key == "u" && self.vim_mode == VimMode::Normal && self.vim_pending == VimPendingState::None {
            if let Some((t, c)) = self.undo_stack.pop() {
                self.text = t;
                self.cursor = c;
                self.normalize();
            }
            return;
        }
        // Enter visual mode with `v` — anchor the selection start
        if key == "v" && self.vim_mode == VimMode::Normal && self.vim_pending == VimPendingState::None {
            self.vim_mode = VimMode::Visual;
            self.visual_anchor = Some(self.cursor);
            return;
        }
        // Enter command-line mode with `:`
        if key == ":" && self.vim_mode == VimMode::Normal && self.vim_pending == VimPendingState::None {
            self.vim_mode = VimMode::Command;
            self.vim_command_buf.clear();
            return;
        }
        // Enter in-prompt search with `/`
        if key == "/" && self.vim_mode == VimMode::Normal && self.vim_pending == VimPendingState::None {
            self.vim_mode = VimMode::Search;
            self.vim_search_buf.clear();
            return;
        }
        // Enter visual-line mode with `V`
        if key == "V" && self.vim_mode == VimMode::Normal && self.vim_pending == VimPendingState::None {
            self.vim_mode = VimMode::VisualLine;
            let ls = self.text[..self.cursor].rfind('\n').map(|p| p + 1).unwrap_or(0);
            self.visual_anchor = Some(ls);
            return;
        }
        // Enter visual-block mode with Ctrl+V
        if key == "\x16" && self.vim_mode == VimMode::Normal && self.vim_pending == VimPendingState::None {
            self.vim_mode = VimMode::VisualBlock;
            self.visual_anchor = Some(self.cursor);
            return;
        }
        // `n` — repeat last search forward
        if key == "n" && self.vim_mode == VimMode::Normal && self.vim_pending == VimPendingState::None {
            if let Some(pat) = self.vim_search_last.clone() {
                self.vim_search_forward(&pat, 1);
            }
            return;
        }
        // `N` — repeat last search backward
        if key == "N" && self.vim_mode == VimMode::Normal && self.vim_pending == VimPendingState::None {
            if let Some(pat) = self.vim_search_last.clone() {
                self.vim_search_backward(&pat);
            }
            return;
        }
        // In visual-line mode, `y`/`d`/`c` operate on whole lines, motion keys extend selection
        if self.vim_mode == VimMode::VisualLine {
            if let Some(anchor) = self.visual_anchor {
                let line_start = |pos: usize, s: &str| -> usize {
                    s[..pos].rfind('\n').map(|p| p + 1).unwrap_or(0)
                };
                let line_end = |pos: usize, s: &str| -> usize {
                    s[pos..].find('\n').map(|p| pos + p + 1).unwrap_or(s.len())
                };
                let sel_start = line_start(anchor.min(self.cursor), &self.text);
                let sel_end = line_end(anchor.max(self.cursor), &self.text);
                match key {
                    "y" => {
                        self.yank_buf = self.text[sel_start..sel_end].to_string();
                        self.cursor = sel_start;
                        self.vim_mode = VimMode::Normal;
                        self.visual_anchor = None;
                        return;
                    }
                    "d" | "x" => {
                        self.push_undo();
                        self.yank_buf = self.text[sel_start..sel_end].to_string();
                        let char_count = self.yank_buf.chars().count();
                        self.text.drain(sel_start..sel_end);
                        self.cursor = sel_start.min(self.text.len());
                        self.vim_mode = VimMode::Normal;
                        self.visual_anchor = None;
                        self.vim_dot_action = Some(DotRepeatAction::DeleteChars { count: char_count });
                        self.normalize();
                        return;
                    }
                    "c" => {
                        self.push_undo();
                        self.yank_buf = self.text[sel_start..sel_end].to_string();
                        self.text.drain(sel_start..sel_end);
                        self.cursor = sel_start;
                        self.vim_mode = VimMode::Insert;
                        self.visual_anchor = None;
                        self.vim_insert_text_before = Some(self.text.clone());
                        self.normalize();
                        return;
                    }
                    _ => {
                        // Motion keys extend the selection (handled by apply_vim_key below)
                    }
                }
            }
        }
        // In visual-block mode, treat like character-wise visual for single-line input
        if self.vim_mode == VimMode::VisualBlock {
            if let Some(anchor) = self.visual_anchor {
                let from = anchor.min(self.cursor);
                let to_excl = anchor.max(self.cursor);
                let to = self.text[to_excl..].char_indices().nth(1).map(|(b,_)| to_excl+b).unwrap_or(self.text.len());
                match key {
                    "y" => {
                        self.yank_buf = self.text[from..to].to_string();
                        self.cursor = from;
                        self.vim_mode = VimMode::Normal;
                        self.visual_anchor = None;
                        return;
                    }
                    "d" | "x" => {
                        self.push_undo();
                        self.yank_buf = self.text[from..to].to_string();
                        let char_count = self.yank_buf.chars().count();
                        self.text.drain(from..to);
                        self.cursor = from.min(self.text.len());
                        self.vim_mode = VimMode::Normal;
                        self.visual_anchor = None;
                        self.vim_dot_action = Some(DotRepeatAction::DeleteChars { count: char_count });
                        self.normalize();
                        return;
                    }
                    "c" => {
                        self.push_undo();
                        self.yank_buf = self.text[from..to].to_string();
                        self.text.drain(from..to);
                        self.cursor = from;
                        self.vim_mode = VimMode::Insert;
                        self.visual_anchor = None;
                        self.vim_insert_text_before = Some(self.text.clone());
                        self.normalize();
                        return;
                    }
                    _ => {}
                }
            }
        }
        // In visual mode, `y`/`d`/`c` operate on the selection, Escape exits
        if self.vim_mode == VimMode::Visual {
            if let Some(anchor) = self.visual_anchor {
                let from = anchor.min(self.cursor);
                let to_excl = anchor.max(self.cursor);
                let to = self.text[to_excl..].char_indices().nth(1).map(|(b,_)| to_excl+b).unwrap_or(self.text.len());
                match key {
                    "y" => {
                        self.yank_buf = self.text[from..to].to_string();
                        self.cursor = from;
                        self.vim_mode = VimMode::Normal;
                        self.visual_anchor = None;
                        return;
                    }
                    "d" | "x" => {
                        self.push_undo();
                        self.yank_buf = self.text[from..to].to_string();
                        // Count chars to delete BEFORE mutating text
                        let char_count = self.yank_buf.chars().count();
                        self.text.drain(from..to);
                        self.cursor = from.min(self.text.len());
                        self.vim_mode = VimMode::Normal;
                        self.visual_anchor = None;
                        self.vim_dot_action = Some(DotRepeatAction::DeleteChars {
                            count: char_count,
                        });
                        self.normalize();
                        return;
                    }
                    "c" => {
                        self.push_undo();
                        self.yank_buf = self.text[from..to].to_string();
                        self.text.drain(from..to);
                        self.cursor = from;
                        self.vim_mode = VimMode::Insert;
                        self.visual_anchor = None;
                        self.vim_insert_text_before = Some(self.text.clone());
                        self.normalize();
                        return;
                    }
                    _ => {
                        // Motion keys still move cursor in visual mode
                    }
                }
            }
        }

        let snapshot_text = self.text.clone();
        let snapshot_cursor = self.cursor;
        let modified = apply_vim_key(
            &mut self.vim_mode,
            &mut self.text,
            &mut self.cursor,
            key,
            &mut self.yank_buf,
            &mut self.vim_pending,
            &mut self.last_find,
        );
        if modified {
            self.undo_stack.push((snapshot_text.clone(), snapshot_cursor));
            if self.undo_stack.len() > 100 {
                self.undo_stack.remove(0);
            }
            // Update dot-repeat for simple modifying commands (normal mode only)
            if was_normal {
                match key {
                    "x" => {
                        self.vim_dot_action = Some(DotRepeatAction::DeleteChars { count: 1 });
                    }
                    "X" => {
                        self.vim_dot_action = Some(DotRepeatAction::DeleteChars { count: 1 });
                    }
                    _ => {}
                }
            }
        }

        // If we just entered insert mode from normal mode, record text snapshot for dot-repeat
        if was_normal && self.vim_mode == VimMode::Insert {
            self.vim_insert_text_before = Some(self.text.clone());
        }

        // Handle `r` replace pending → after confirm, store dot action
        if let VimPendingState::None = self.vim_pending {
            if modified && was_normal {
                // Check if a replace happened (text changed by exactly 1 char at cursor)
                if self.text.len() == prev_text_len && self.text != snapshot_text {
                    // Likely a replace — extract the replacement char at snapshot_cursor
                    if let Some(ch) = self.text[snapshot_cursor..].chars().next() {
                        // Verify it's different from what was there before
                        let old_ch = snapshot_text[snapshot_cursor..].chars().next();
                        if old_ch != Some(ch) {
                            self.vim_dot_action = Some(DotRepeatAction::ReplaceChar { ch });
                        }
                    }
                }
            }
        }

        // Update visual anchor tracking when in visual mode
        if (self.vim_mode == VimMode::Visual || self.vim_mode == VimMode::VisualBlock) && self.visual_anchor.is_none() {
            self.visual_anchor = Some(self.cursor);
        }
        self.normalize();
    }

    /// Push the current (text, cursor) to the undo stack.
    pub fn push_undo(&mut self) {
        self.undo_stack.push((self.text.clone(), self.cursor));
        if self.undo_stack.len() > 100 {
            self.undo_stack.remove(0);
        }
    }

    // ---- Named registers ----

    /// Store `text` in the named register `register`.
    pub fn yank_to_register(&mut self, register: char, text: &str) {
        self.vim_registers.insert(register, text.to_string());
    }

    /// Retrieve text from the named register `register`, if any.
    pub fn paste_from_register(&mut self, register: char) -> Option<String> {
        self.vim_registers.get(&register).cloned()
    }

    // ---- Marks ----

    /// Set mark `name` at the current cursor position.
    pub fn set_mark(&mut self, name: char) {
        self.vim_marks.insert(name, (self.text.clone(), self.cursor));
    }

    /// Move cursor to the position recorded for mark `name`, if the text still matches.
    pub fn jump_to_mark(&mut self, name: char) {
        if let Some((_saved_text, saved_cursor)) = self.vim_marks.get(&name).cloned() {
            // Clamp to current text length in case text changed.
            let target = saved_cursor.min(self.text.len());
            // Ensure we land on a char boundary.
            let mut pos = target;
            while pos > 0 && !self.text.is_char_boundary(pos) {
                pos -= 1;
            }
            self.cursor = pos;
        }
    }

    // ---- Macro recording ----

    /// Begin recording a macro into register `register`.
    /// If already recording, stops the current recording first.
    pub fn start_macro_recording(&mut self, register: char) {
        self.vim_macro_recording = Some(register);
        self.vim_macro_content.insert(register, Vec::new());
    }

    /// Stop recording the current macro. Returns the register name that was being recorded.
    pub fn stop_macro_recording(&mut self) -> Option<char> {
        self.vim_macro_recording.take()
    }

    /// Return the recorded key sequence for `register`, or an empty vec.
    pub fn replay_macro(&self, register: char) -> Vec<String> {
        self.vim_macro_content.get(&register).cloned().unwrap_or_default()
    }

    // ---- Vim command-line execution ----

    /// Execute a `:` command-line command.
    /// Recognised: `q`/`quit`, `wq`, `set` (no-op), `noh` (clear search highlight).
    pub fn execute_vim_cmdline(&mut self, cmd: &str) {
        match cmd {
            "q" | "quit" | "wq" | "x" => {
                // In prompt context we can only signal quit by clearing + a special flag.
                // We set a dedicated field that the app loop can inspect.
                self.vim_quit_requested = true;
            }
            "noh" | "nohlsearch" => {
                self.vim_search_last = None;
            }
            s if s.starts_with("set ") => {
                // `:set vim` → enable, `:set novim` → disable (runtime toggle)
                let arg = s["set ".len()..].trim();
                match arg {
                    "vim" => { self.vim_enabled = true; }
                    "novim" => { self.vim_enabled = false; }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    // ---- In-prompt search ----

    /// Move cursor to the next occurrence of `pattern` after `cursor + skip`.
    /// `skip = 0` finds from current position; `skip = 1` finds the *next* one.
    pub fn vim_search_forward(&mut self, pattern: &str, skip: usize) {
        if pattern.is_empty() { return; }
        let start = if skip > 0 {
            // Start after the current character to avoid re-matching same position
            let next = self.text[self.cursor..].char_indices().nth(1)
                .map(|(b, _)| self.cursor + b)
                .unwrap_or(0);
            next
        } else {
            self.cursor
        };
        // Search from `start` forward, then wrap around
        let text_lc = self.text.to_lowercase();
        let pat_lc = pattern.to_lowercase();
        if let Some(pos) = text_lc[start..].find(&pat_lc) {
            self.cursor = start + pos;
            return;
        }
        // Wrap: search from beginning
        if let Some(pos) = text_lc.find(&pat_lc) {
            self.cursor = pos;
        }
    }

    /// Move cursor to the previous occurrence of `pattern` before current cursor.
    pub fn vim_search_backward(&mut self, pattern: &str) {
        if pattern.is_empty() { return; }
        let text_lc = self.text.to_lowercase();
        let pat_lc = pattern.to_lowercase();
        // Find all occurrences, pick the last one before cursor
        let before = &text_lc[..self.cursor];
        if let Some(pos) = before.rfind(&pat_lc) {
            self.cursor = pos;
            return;
        }
        // Wrap: find last occurrence in whole text
        if let Some(pos) = text_lc.rfind(&pat_lc) {
            self.cursor = pos;
        }
    }

    /// Clear the input and reset state.
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.suggestions.clear();
        self.suggestion_index = None;
        self.history_pos = None;
        self.token_estimate = 0;
        self.vim_pending = VimPendingState::None;
        self.visual_anchor = None;
        self.vim_command_buf.clear();
        self.vim_search_buf.clear();
        // #223: emptying the buffer removes every `[Pasted text #N ...]`
        // placeholder along with it, so the paste bodies stored in
        // `paste_contents` are now unreachable. Drop them to bound memory —
        // otherwise every large block ever pasted is retained for the App's
        // lifetime. `clear()` is the funnel for submit (`take()`), cancel, and
        // reset, so eviction only ever fires once a placeholder has left the
        // buffer, never while one is still live (so expansion is never broken).
        self.paste_contents.clear();
    }

    /// Take the current text, clearing the input. Paste placeholders are
    /// expanded back into their stored bodies so the submitted message carries
    /// the real pasted content, not the `[Pasted text #N ...]` stand-in.
    pub fn take(&mut self) -> String {
        let text = self.expanded_text();
        self.clear();
        text
    }

    /// The submit-ready text: every `[Pasted text #N]` / `[Pasted text #N +X
    /// lines]` placeholder replaced with its stored body. Placeholders whose
    /// body is no longer stored (e.g. hand-typed or edited references) are
    /// left as-is. Replacements run in reverse order so earlier byte offsets
    /// stay valid.
    pub fn expanded_text(&self) -> String {
        let mut out = self.text.clone();
        if self.paste_contents.is_empty() {
            return out;
        }
        let refs = claurst_core::prompt_history::parse_references_with_positions(&self.text);
        for (id, matched, start) in refs.into_iter().rev() {
            if !matched.starts_with("[Pasted text #") {
                continue;
            }
            if let Some(body) = self.paste_contents.get(&id) {
                out.replace_range(start..start + matched.len(), body);
            }
        }
        out
    }

    /// True when the buffer contains at least one paste placeholder that can
    /// still be expanded (its body is stored). Drives the footer hint.
    pub fn has_expandable_paste_ref(&self) -> bool {
        if self.paste_contents.is_empty() {
            return false;
        }
        claurst_core::prompt_history::parse_references_with_positions(&self.text)
            .iter()
            .any(|(id, matched, _)| {
                matched.starts_with("[Pasted text #") && self.paste_contents.contains_key(id)
            })
    }

    /// Expand the paste placeholder covering byte offset `offset` (start
    /// inclusive, end inclusive so the cursor sitting just past the `]` still
    /// counts) back into its stored body, in place. The cursor lands at the
    /// end of the inserted body. Returns `true` if a placeholder was expanded.
    pub fn expand_paste_ref_at(&mut self, offset: usize) -> bool {
        if self.mode == InputMode::Readonly {
            return false;
        }
        let refs = claurst_core::prompt_history::parse_references_with_positions(&self.text);
        for (id, matched, start) in refs {
            if !matched.starts_with("[Pasted text #") {
                continue;
            }
            let end = start + matched.len();
            if offset < start || offset > end {
                continue;
            }
            let Some(body) = self.paste_contents.remove(&id) else {
                continue;
            };
            self.push_undo();
            self.text.replace_range(start..end, &body);
            self.cursor = start + body.len();
            self.update_token_estimate();
            return true;
        }
        false
    }

    /// The paste placeholder covering byte offset `offset` (same hit rules as
    /// `expand_paste_ref_at`) together with its stored body, without mutating
    /// the buffer — used by the read-only paste viewer.
    pub fn paste_ref_at(&self, offset: usize) -> Option<(u32, String)> {
        let refs = claurst_core::prompt_history::parse_references_with_positions(&self.text);
        for (id, matched, start) in refs {
            if !matched.starts_with("[Pasted text #") {
                continue;
            }
            let end = start + matched.len();
            if offset < start || offset > end {
                continue;
            }
            if let Some(body) = self.paste_contents.get(&id) {
                return Some((id, body.clone()));
            }
        }
        None
    }

    /// Expand the paste placeholder at the cursor; falls back to the first
    /// expandable placeholder in the buffer so the keybinding works without
    /// first navigating onto the reference.
    pub fn expand_paste_ref_at_cursor(&mut self) -> bool {
        if self.expand_paste_ref_at(self.cursor) {
            return true;
        }
        let first = claurst_core::prompt_history::parse_references_with_positions(&self.text)
            .into_iter()
            .find(|(id, matched, _)| {
                matched.starts_with("[Pasted text #") && self.paste_contents.contains_key(id)
            });
        match first {
            Some((_, _, start)) => self.expand_paste_ref_at(start),
            None => false,
        }
    }

    /// Returns true if the text (up to cursor) contains a word-boundary `@` token,
    /// meaning an `@file` reference is actively being typed.
    pub fn has_active_file_ref(&self) -> bool {
        let text = &self.text[..self.cursor];
        text.rfind('@').is_some_and(|at_idx| {
            at_idx == 0 || text[..at_idx].chars().last().is_some_and(|c| c.is_whitespace())
        })
    }

    /// Update typeahead suggestions for slash commands and file references in the current text.
    pub fn update_suggestions(&mut self, slash_commands: &[(&str, &str)], file_autocomplete_limit: usize, file_autocomplete_show_hidden: bool) {
        // Only look at text up to the cursor — text after the cursor belongs to a
        // different editing position and would confuse rfind('@') / rfind('/').
        let text_before_cursor = &self.text[..self.cursor];
        self.suggestions = compute_typeahead(text_before_cursor, slash_commands, file_autocomplete_limit, file_autocomplete_show_hidden);

        if self.suggestions.is_empty() {
            self.suggestion_index = None;
        } else {
            let idx = self.suggestion_index.unwrap_or(0).min(self.suggestions.len() - 1);
            self.suggestion_index = Some(idx);
        }
    }

    /// Select the next suggestion.
    pub fn suggestion_next(&mut self) {
        if self.suggestions.is_empty() { return; }
        self.suggestion_index = Some(
            self.suggestion_index.map_or(0, |i| (i + 1) % self.suggestions.len())
        );
    }

    /// Select the previous suggestion.
    pub fn suggestion_prev(&mut self) {
        if self.suggestions.is_empty() { return; }
        self.suggestion_index = Some(
            self.suggestion_index
                .map_or(0, |i| if i == 0 { self.suggestions.len() - 1 } else { i - 1 })
        );
    }

    /// Accept the current suggestion.
    pub fn accept_suggestion(&mut self) {
        if let Some(idx) = self.suggestion_index {
            if let Some(s) = self.suggestions.get(idx) {
                let new_cursor = match s.source {
                    TypeaheadSource::SlashCommand | TypeaheadSource::History => {
                        // Replace entire text; discard anything after cursor too.
                        self.text = s.text.clone();
                        self.text.len()
                    }
                    TypeaheadSource::FileRef => {
                        // Replace from the last word-boundary @ up to the cursor.
                        // Preserve any text that was already after the cursor.
                        let tail = self.text[self.cursor..].to_string();
                        if let Some(at_idx) = self.text[..self.cursor].rfind('@') {
                            let at_word_boundary = at_idx == 0
                                || self.text[..at_idx]
                                    .chars()
                                    .last()
                                    .map(|c| c.is_whitespace())
                                    .unwrap_or(false);
                            if at_word_boundary {
                                let mut new_text = self.text[..at_idx].to_string();
                                new_text.push_str(&s.text);
                                let cursor = new_text.len();
                                new_text.push_str(&tail);
                                self.text = new_text;
                                cursor
                            } else {
                                let mut new_text = s.text.clone();
                                let cursor = new_text.len();
                                new_text.push_str(&tail);
                                self.text = new_text;
                                cursor
                            }
                        } else {
                            let mut new_text = s.text.clone();
                            let cursor = new_text.len();
                            new_text.push_str(&tail);
                            self.text = new_text;
                            cursor
                        }
                    }
                };
                self.cursor = new_cursor;
                self.suggestions.clear();
                self.suggestion_index = None;
                self.update_token_estimate();
            }
        }
    }

    /// Replace the full text buffer and move the cursor to the end.
    pub fn replace_text(&mut self, text: String) {
        self.text = text;
        self.cursor = self.text.len();
        self.history_pos = None;
        self.suggestion_index = None;
        self.update_token_estimate();
    }

    /// Map the current cursor (byte offset) to a (visual_row, visual_col) pair
    /// given the wrap width. `width` is the usable column count for text.
    pub fn cursor_visual_pos(&self, width: usize) -> (usize, usize) {
        if width == 0 {
            return (0, 0);
        }
        let mut byte = 0usize;
        let mut row = 0usize;
        for line in self.text.split('\n') {
            let line_end = byte + line.len();
            if self.cursor <= line_end {
                let intra_byte = self.cursor - byte;
                let intra_byte = intra_byte.min(line.len());
                // walk to char-boundary
                let mut b = intra_byte;
                while b > 0 && !line.is_char_boundary(b) {
                    b -= 1;
                }
                let display_col = UnicodeWidthStr::width(&line[..b]);
                let chunk_idx = if display_col == 0 { 0 } else { display_col / width };
                let chunk_col = display_col % width;
                return (row + chunk_idx, chunk_col);
            }
            let chunks = wrap_line(line, width).len().max(1);
            row += chunks;
            byte = line_end + 1; // newline
        }
        (row.saturating_sub(1), 0)
    }

    /// Move the cursor to the same visual column on the row above. Returns
    /// `true` if the cursor actually moved (i.e. there was a row above).
    pub fn move_visual_up(&mut self, width: usize) -> bool {
        if width == 0 {
            return false;
        }
        let (row, col) = self.cursor_visual_pos(width);
        if row == 0 {
            return false;
        }
        self.set_cursor_at_visual(row - 1, col, width);
        true
    }

    /// Move the cursor to the same visual column on the row below. Returns
    /// `true` if the cursor actually moved (i.e. there was a row below).
    pub fn move_visual_down(&mut self, width: usize) -> bool {
        if width == 0 {
            return false;
        }
        let (row, col) = self.cursor_visual_pos(width);
        let total_rows = self.visual_row_count(width);
        if row + 1 >= total_rows {
            return false;
        }
        self.set_cursor_at_visual(row + 1, col, width);
        true
    }

    pub(crate) fn visual_row_count(&self, width: usize) -> usize {
        if self.text.is_empty() || width == 0 {
            return 1;
        }
        let mut total = 0usize;
        for line in self.text.split('\n') {
            total += wrap_line(line, width).len().max(1);
        }
        total.max(1)
    }

    pub(crate) fn set_cursor_at_visual(&mut self, target_row: usize, target_col: usize, width: usize) {
        if width == 0 {
            return;
        }
        let mut byte = 0usize;
        let mut row = 0usize;
        for line in self.text.split('\n').collect::<Vec<_>>() {
            let chunks = wrap_line(line, width).len().max(1);
            if target_row < row + chunks {
                let intra_chunk = target_row - row;
                let chunk_char_start = intra_chunk * width;
                let line_chars: Vec<(usize, char)> = line.char_indices().collect();
                let chunk_chars_len = line_chars
                    .len()
                    .saturating_sub(chunk_char_start)
                    .min(width);
                let col = target_col.min(chunk_chars_len);
                let target_char_idx = chunk_char_start + col;
                let intra_byte = line_chars
                    .get(target_char_idx)
                    .map(|(b, _)| *b)
                    .unwrap_or(line.len());
                self.cursor = byte + intra_byte;
                self.history_pos = None;
                return;
            }
            row += chunks;
            byte += line.len() + 1; // newline
        }
        self.cursor = self.text.len();
        self.history_pos = None;
    }

    /// Normalize cursor and metadata after external field updates.
    pub fn normalize(&mut self) {
        self.cursor = self.cursor.min(self.text.len());
        while self.cursor > 0 && !self.text.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
        self.update_token_estimate();
    }

    /// Rough token estimate: ~4 chars per token.
    fn update_token_estimate(&mut self) {
        self.token_estimate = self.text.len().div_ceil(4);
    }

    pub fn is_empty(&self) -> bool { self.text.trim().is_empty() }
}

impl Default for PromptInputState {
    fn default() -> Self { Self::new() }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Return the number of rows needed to render the input for the given text.
/// `text_width` is the usable column count for wrapped text (i.e. area.width
/// minus the prompt prefix and right margin). When 0 we degrade gracefully and
/// only count logical lines.
///
/// Issue #149 follow-up: previously this only counted `\n`-separated lines, so
/// a single long visually-wrapped line stayed at the minimum height. Now we
/// count the actual visual row count and grow the box up to ~10 text rows
/// (12 total including the underline + breathing room). Larger inputs scroll
/// inside the box (handled in `render_prompt_input`).
pub fn input_height(state: &PromptInputState, text_width: u16) -> u16 {
    let visual_lines = if state.text.is_empty() {
        1usize
    } else if text_width == 0 {
        state.text.lines().count().max(1)
    } else {
        let mut total = 0usize;
        let logical: Vec<&str> = state.text.split('\n').collect();
        for line in &logical {
            let chunks = wrap_line(line, text_width as usize).len().max(1);
            total += chunks;
        }
        total.max(1)
    };
    // top-line + text rows + breathing room + underline, capped so the prompt
    // never eats more than ~half the screen.
    const MAX_TEXT_ROWS: usize = 10;
    let text_rows = visual_lines.min(MAX_TEXT_ROWS) as u16;
    let base = (text_rows + 3).max(4);
    base + if state.pending_images.is_empty() { 0 } else { 1 }
}

/// Wrap a logical line into visual chunks of `width` terminal cells. Empty
/// input yields a single empty chunk so the caller can still place a cursor.
pub fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![line.to_string()];
    }
    if line.is_empty() {
        return vec![String::new()];
    }

    let mut out = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for ch in line.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if current_width > 0 && current_width + ch_width > width {
            out.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width += ch_width;
    }
    if !current.is_empty() {
        out.push(current);
    }

    out
}

/// Themed per-character gradient for an inline keyword's prompt-box highlight.
///
/// Each keyword gets its own two-color fade painted one span per character
/// (bold), matching the quality of the original ultracode highlight.
#[derive(Clone, Copy)]
struct KeywordGradient {
    start: (u8, u8, u8),
    end: (u8, u8, u8),
    /// When true, apply a small deterministic per-character offset so the run
    /// looks textured/dithered (stony) rather than a smooth fade — used by
    /// `rocky` for a granite look.
    dither: bool,
}

/// Claurst-red gradient for `ultracode` (matches the `/effort` selector's red
/// theme): the signature red fading into a deeper red. Unchanged.
const ULTRACODE_GRADIENT: KeywordGradient = KeywordGradient {
    start: (233, 30, 99),
    end: (140, 20, 45),
    dither: false,
};

/// Stony grey gradient for the `rocky` persona — light granite fading to dark
/// stone, dithered per character for a textured rock face.
const ROCKY_GRADIENT: KeywordGradient = KeywordGradient {
    start: (176, 176, 182),
    end: (92, 92, 100),
    dither: true,
};

/// Earthy gradient for the `caveman` persona — tree-root brown fading to leaf
/// green.
const CAVEMAN_GRADIENT: KeywordGradient = KeywordGradient {
    start: (120, 78, 42),
    end: (74, 150, 58),
    dither: false,
};

/// The themed gradient for a keyword, or `None` if it should not be painted
/// (e.g. `normal`, which is a reset and gets the default no-gradient look).
fn keyword_gradient_for(keyword: &str) -> Option<KeywordGradient> {
    match keyword.to_ascii_lowercase().as_str() {
        "ultracode" => Some(ULTRACODE_GRADIENT),
        "rocky" => Some(ROCKY_GRADIENT),
        "caveman" => Some(CAVEMAN_GRADIENT),
        _ => None,
    }
}

/// Linearly interpolate between two RGB colors at `t` in `[0, 1]`, returning the
/// raw channels so callers can post-process (e.g. dither) before wrapping in a
/// `Color`.
fn keyword_lerp_rgb(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

/// The color for character index `i` of a keyword run at gradient position `t`.
fn keyword_gradient_color(grad: &KeywordGradient, t: f32, i: usize) -> Color {
    let (mut r, mut g, mut b) = keyword_lerp_rgb(grad.start, grad.end, t);
    if grad.dither {
        // Deterministic per-character speckle so adjacent stone glyphs differ
        // slightly, giving a dithered granite texture instead of a smooth fade.
        let d: i16 = match i % 3 {
            0 => 20,
            1 => -16,
            _ => 8,
        };
        let jitter = |c: u8| (c as i16 + d).clamp(0, 255) as u8;
        r = jitter(r);
        g = jitter(g);
        b = jitter(b);
    }
    Color::Rgb(r, g, b)
}

/// Build the styled spans for a single display-line `text`, painting every
/// inline keyword occurrence (`ultracode`, `rocky`, `caveman` — the single
/// word, case-insensitive, whole-word) with its own themed per-character
/// gradient, bold. `normal` is a reset and is intentionally not painted.
///
/// VISUAL ONLY: the returned spans concatenate back to exactly `text`, in order,
/// with identical characters and widths -- only the *style* differs on keyword
/// characters. Text outside any keyword is emitted as a single span carrying
/// `base_style`, so cursor positioning, wrapping, and editing are unaffected.
pub(crate) fn styled_spans_with_keyword_gradient(
    text: &str,
    base_style: Style,
) -> Vec<Span<'static>> {
    // Collect (start, end, gradient) for every gradient-bearing keyword.
    let mut matches: Vec<(usize, usize, KeywordGradient)> = Vec::new();
    for kw in claurst_core::keywords::INLINE_KEYWORDS {
        if !kw.gradient {
            continue;
        }
        let Some(grad) = keyword_gradient_for(kw.keyword) else {
            continue;
        };
        for (start, end) in claurst_core::keywords::keyword_match_ranges(text, kw.keyword) {
            matches.push((start, end, grad));
        }
    }
    if matches.is_empty() {
        return vec![Span::styled(text.to_string(), base_style)];
    }
    matches.sort_by_key(|(start, _, _)| *start);

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut pos = 0usize;
    for (start, end, grad) in matches {
        // Defensive: keywords are mutually non-overlapping, but skip any stray
        // overlap so spans always reassemble to exactly `text`.
        if start < pos {
            continue;
        }
        if start > pos {
            spans.push(Span::styled(text[pos..start].to_string(), base_style));
        }
        let keyword = &text[start..end];
        let char_count = keyword.chars().count().max(1);
        for (i, ch) in keyword.chars().enumerate() {
            let t = if char_count == 1 {
                0.0
            } else {
                i as f32 / (char_count - 1) as f32
            };
            let color = keyword_gradient_color(&grad, t, i);
            spans.push(Span::styled(
                ch.to_string(),
                base_style.fg(color).add_modifier(Modifier::BOLD),
            ));
        }
        pos = end;
    }
    if pos < text.len() {
        spans.push(Span::styled(text[pos..].to_string(), base_style));
    }
    spans
}

/// Render the prompt input widget in the same low-chrome style as Claurst:
/// multi-line input rows (one per logical line in the text) plus an accent
/// underline. Suggestions are rendered by the footer, not as a boxed dropdown
/// here.
pub fn render_prompt_input(
    state: &PromptInputState,
    area: Rect,
    buf: &mut Buffer,
    focused: bool,
    mode: InputMode,
    accent_override: Color,
    cursor_blink_enabled: bool,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    // If images are pending, render a pill row above everything else and shrink area.
    let (area, image_row_y) = if !state.pending_images.is_empty() && area.height > 1 {
        let pill_y = area.y;
        let rest = Rect { x: area.x, y: area.y + 1, width: area.width, height: area.height - 1 };
        (rest, Some(pill_y))
    } else {
        (area, None)
    };

    if let Some(pill_y) = image_row_y {
        let mut pills: Vec<Span<'static>> = Vec::new();
        for img in &state.pending_images {
            let label = if let Some((w, h)) = img.dimensions {
                format!(" \u{f03e} {} {}x{} ", img.label, w, h)  // nerd-font image icon, fallback to plain text
            } else {
                format!(" \u{f03e} {} ", img.label)
            };
            pills.push(Span::styled(label, Style::default().fg(Color::Black).bg(Color::Cyan)));
            pills.push(Span::raw(" "));
        }
        if !pills.is_empty() {
            Paragraph::new(Line::from(pills))
                .render(Rect { x: area.x, y: pill_y, width: area.width, height: 1 }, buf);
        }
    }

    let accent = match mode {
        InputMode::Readonly => CLAUDE_ORANGE,   // locked while streaming — always pink
        _ => accent_override,                   // use mode-aware accent color
    };
    let prompt_prefix = format!("{PROMPT_POINTER} ");
    let prefix_width = UnicodeWidthStr::width(prompt_prefix.as_str()) as u16;
    // Reserve a 2-cell right margin so wrapped text doesn't kiss the right edge
    // of the box (issue #149: padding too tight).
    let right_pad: u16 = 2;
    let available_width = area
        .width
        .saturating_sub(prefix_width)
        .saturating_sub(right_pad) as usize;
    let cursor_visible = if cursor_blink_enabled {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        (ms / 530).is_multiple_of(2)
    } else {
        true
    };
    // Render cursor as an overlay so its blink state never shifts the
    // underlying text (issue #149: cursor blink shifted the prompt).
    let show_cursor = focused && cursor_visible;

    // Use the raw text — no inline cursor character — so layout is stable.
    let display_text: String = if state.text.is_empty() {
        if focused {
            String::new()
        } else if mode == InputMode::Default {
            "How can I help you?".to_string()
        } else {
            String::new()
        }
    } else {
        state.text.clone()
    };

    // Top separator line (matches bottom underline — visual "box" around the prompt).
    if area.height > 0 {
        Paragraph::new(Line::from(vec![Span::styled(
            "\u{2500}".repeat(area.width as usize),
            Style::default().fg(accent),
        )]))
        .render(Rect { x: area.x, y: area.y, width: area.width, height: 1 }, buf);
    }

    // Text rows start 1 row below the top separator.
    let text_start_y = area.y + 1;

    // Split into logical lines on '\n'. Using `split` (not `.lines()`, which
    // strips '\r') keeps the byte accounting below in sync with the buffer:
    // each logical line is separated by exactly one '\n' byte, and text ending
    // in '\n' naturally yields a trailing empty line. `split` always yields at
    // least one element, so there is always >= 1 logical line (#221).
    let logical_lines: Vec<String> = display_text.split('\n').map(|l| l.to_string()).collect();

    let text_style = if state.text.is_empty() && !focused {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::White)
    };

    // Wrap each logical line into visual rows that fit `available_width`,
    // and remember the (logical_idx, intra_line_display_col) for each row
    // so we can later compute where the cursor lives.
    let mut visual_rows: Vec<(usize, usize, String)> = Vec::new();
    for (li, line_text) in logical_lines.iter().enumerate() {
        let chunks = wrap_line(line_text, available_width.max(1));
        let mut col_offset = 0usize;
        for chunk in chunks {
            let chunk_len = UnicodeWidthStr::width(chunk.as_str());
            visual_rows.push((li, col_offset, chunk));
            col_offset += chunk_len;
        }
    }

    // Compute cursor's visual (row, col) within `visual_rows`.
    // We map state.cursor (a byte offset into state.text) to
    // (logical_line, display column).
    let cursor_pos: Option<(usize, usize)> = if focused && !state.text.is_empty() {
        let mut byte_idx = 0usize;
        let mut found: Option<(usize, usize)> = None;
        'outer: for (li, line_text) in logical_lines.iter().enumerate() {
            let line_bytes = line_text.len();
            // The +1 accounts for the '\n' between logical lines (last line has no trailing \n).
            let line_end_byte = byte_idx + line_bytes;
            if state.cursor <= line_end_byte {
                // Clamp the cursor into this line, then walk back to a char
                // boundary so a multibyte codepoint straddling the cut never
                // panics the slice (#221).
                let mut intra_byte = (state.cursor - byte_idx).min(line_bytes);
                while intra_byte > 0 && !line_text.is_char_boundary(intra_byte) {
                    intra_byte -= 1;
                }
                let display_col = UnicodeWidthStr::width(&line_text[..intra_byte]);
                found = Some((li, display_col));
                break 'outer;
            }
            byte_idx = line_end_byte + 1; // newline
        }
        // Fallback: cursor at end of text.
        found.or_else(|| {
            let li = logical_lines.len().saturating_sub(1);
            let col = logical_lines
                .get(li)
                .map(|s| UnicodeWidthStr::width(s.as_str()))
                .unwrap_or(0);
            Some((li, col))
        })
    } else if focused && state.text.is_empty() {
        Some((0, 0))
    } else {
        None
    };

    let cursor_visual: Option<(usize, usize)> = cursor_pos.and_then(|(li, col)| {
        // Find the visual row whose logical_idx == li and contains `col`.
        let mut last_match: Option<(usize, usize)> = None;
        for (vi, (row_li, row_col_start, chunk)) in visual_rows.iter().enumerate() {
            if *row_li != li {
                continue;
            }
            let chunk_len = UnicodeWidthStr::width(chunk.as_str());
            let row_col_end = row_col_start + chunk_len;
            if col >= *row_col_start && col <= row_col_end {
                last_match = Some((vi, col - row_col_start));
            }
        }
        last_match
    });

    // Render each visual row (truncated to area height).
    let max_text_rows = area.height.saturating_sub(2) as usize;
    // Scroll so the cursor row is visible.
    let scroll_offset = match cursor_visual {
        Some((vi, _)) if visual_rows.len() > max_text_rows && vi >= max_text_rows => {
            vi + 1 - max_text_rows
        }
        _ => 0,
    };

    for (display_idx, (vi, (li, _col_start, chunk))) in visual_rows
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(max_text_rows)
        .map(|(idx, item)| (idx - scroll_offset, item))
        .enumerate()
        .map(|(d, (idx, item))| (d, (idx + scroll_offset, item)))
    {
        let _ = vi;
        let _ = li;
        let row_y = text_start_y + display_idx as u16;

        // Determine if this is the first visual row of the first logical line —
        // that's the only row that gets the prompt prefix; continuation rows
        // (whether from logical line breaks or wrapping) get whitespace.
        let is_first_row_of_first_logical = display_idx == 0 && scroll_offset == 0;

        let spans: Vec<Span<'static>> = if is_first_row_of_first_logical {
            let mut v = vec![Span::styled(
                prompt_prefix.clone(),
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            )];
            v.extend(styled_spans_with_keyword_gradient(chunk, text_style));
            v
        } else {
            let mut v = vec![Span::raw(" ".repeat(prefix_width as usize))];
            v.extend(styled_spans_with_keyword_gradient(chunk, text_style));
            v
        };

        Paragraph::new(Line::from(spans)).render(
            Rect { x: area.x, y: row_y, width: area.width, height: 1 },
            buf,
        );
    }

    // Overlay the cursor block on top of the rendered text. We modify the
    // buffer cell directly so the cursor occupies the same column whether
    // it is currently blinking on or off.
    if show_cursor {
        if let Some((vi, col_in_row)) = cursor_visual {
            if vi >= scroll_offset {
                let display_idx = vi - scroll_offset;
                if display_idx < max_text_rows {
                    let row_y = text_start_y + display_idx as u16;
                    let x = area.x + prefix_width + col_in_row as u16;
                    if x < area.x + area.width && row_y < area.y + area.height {
                        let cell = &mut buf[(x, row_y)];
                        cell.set_symbol("\u{2588}");
                        cell.set_style(Style::default().fg(text_style.fg.unwrap_or(Color::White)));
                    }
                }
            }
        }
    }

    // Vim command / search row (shown below text lines, before underline).
    let text_rows_rendered = visual_rows
        .len()
        .saturating_sub(scroll_offset)
        .min(max_text_rows);
    let cmd_line: Option<Line<'static>> = match state.vim_mode {
        VimMode::Command => {
            let buf_text = format!(":{}\u{2588}", state.vim_command_buf);
            Some(Line::from(vec![Span::styled(buf_text, Style::default().fg(Color::Cyan))]))
        }
        VimMode::Search => {
            let buf_text = format!("/{}\u{2588}", state.vim_search_buf);
            Some(Line::from(vec![Span::styled(buf_text, Style::default().fg(Color::Yellow))]))
        }
        _ => None,
    };

    let (cmdline_row, underline_row) = if let Some(ref _cl) = cmd_line {
        let cmd_y = text_start_y + text_rows_rendered as u16;
        let ul_y = cmd_y + 1;
        (Some(cmd_y), ul_y)
    } else {
        (None, text_start_y + text_rows_rendered as u16)
    };

    if let (Some(row), Some(cl)) = (cmdline_row, cmd_line) {
        if row < area.y + area.height {
            Paragraph::new(cl).render(
                Rect { x: area.x, y: row, width: area.width, height: 1 },
                buf,
            );
        }
    }

    if underline_row < area.y + area.height {
        Paragraph::new(Line::from(vec![Span::styled(
            "\u{2500}".repeat(area.width as usize),
            Style::default().fg(accent),
        )]))
        .render(
            Rect { x: area.x, y: underline_row, width: area.width, height: 1 },
            buf,
        );
    }

    // Token estimate overlay on the first text row (top-right corner).
    // Format mirrors TS formatTokens: compact "1.3k" for ≥1000, raw number below that.
    if state.text.len() > 1000 && area.height > 1 {
        let n = state.token_estimate;
        let formatted = if n >= 1000 {
            let k = n as f64 / 1000.0;
            // One decimal place, suppress trailing ".0" (e.g. 2000 → "2k", 1300 → "1.3k")
            if (k * 10.0).round() % 10.0 == 0.0 {
                format!("~{}k", k as u64)
            } else {
                format!("~{:.1}k", k)
            }
        } else {
            format!("~{}", n)
        };
        let count_str = formatted;
        let x = area.x + area.width.saturating_sub(count_str.len() as u16);
        Paragraph::new(Line::from(vec![Span::styled(
            count_str,
            Style::default().fg(Color::DarkGray),
        )]))
        .render(
            Rect {
                x,
                y: text_start_y,
                width: area.width.saturating_sub(x.saturating_sub(area.x)),
                height: 1,
            },
            buf,
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    // Several tests are named after the vim key they exercise (e.g. `W`, `G`,
    // `N`); the uppercase letters are meaningful, matching the motion helpers
    // above that already carry `#[allow(non_snake_case)]`.
    #![allow(non_snake_case)]
    use super::*;

    // ---- VimMode --------------------------------------------------------

    #[test]
    fn vim_mode_labels() {
        assert_eq!(VimMode::Insert.label(), "INSERT");
        assert_eq!(VimMode::Normal.label(), "NORMAL");
        assert_eq!(VimMode::Visual.label(), "VISUAL");
    }

    #[test]
    fn vim_insert_to_normal_via_escape() {
        let mut mode = VimMode::Insert;
        let mut text = "hello".to_string();
        let mut cursor = 3;
        let mut yank = String::new();
        apply_vim_command(&mut mode, &mut text, &mut cursor, "Escape", &mut yank);
        assert_eq!(mode, VimMode::Normal);
    }

    #[test]
    fn vim_normal_i_enters_insert() {
        let mut mode = VimMode::Normal;
        let mut text = "hello".to_string();
        let mut cursor = 0;
        let mut yank = String::new();
        apply_vim_command(&mut mode, &mut text, &mut cursor, "i", &mut yank);
        assert_eq!(mode, VimMode::Insert);
        assert_eq!(cursor, 0);
    }

    #[test]
    fn vim_normal_a_goes_to_end() {
        let mut mode = VimMode::Normal;
        let mut text = "hello".to_string();
        let mut cursor = 0;
        let mut yank = String::new();
        apply_vim_command(&mut mode, &mut text, &mut cursor, "A", &mut yank);
        assert_eq!(mode, VimMode::Insert);
        assert_eq!(cursor, 5);
    }

    #[test]
    fn vim_h_moves_left() {
        let mut mode = VimMode::Normal;
        let mut text = "hello".to_string();
        let mut cursor = 3;
        let mut yank = String::new();
        apply_vim_command(&mut mode, &mut text, &mut cursor, "h", &mut yank);
        assert_eq!(cursor, 2);
    }

    #[test]
    fn vim_l_moves_right() {
        let mut mode = VimMode::Normal;
        let mut text = "hello".to_string();
        let mut cursor = 2;
        let mut yank = String::new();
        apply_vim_command(&mut mode, &mut text, &mut cursor, "l", &mut yank);
        assert_eq!(cursor, 3);
    }

    #[test]
    fn vim_dollar_goes_to_end() {
        let mut mode = VimMode::Normal;
        let mut text = "hello".to_string();
        let mut cursor = 0;
        let mut yank = String::new();
        apply_vim_command(&mut mode, &mut text, &mut cursor, "$", &mut yank);
        assert_eq!(cursor, 5);
    }

    #[test]
    fn vim_zero_goes_to_start() {
        let mut mode = VimMode::Normal;
        let mut text = "hello".to_string();
        let mut cursor = 4;
        let mut yank = String::new();
        apply_vim_command(&mut mode, &mut text, &mut cursor, "0", &mut yank);
        assert_eq!(cursor, 0);
    }

    #[test]
    fn vim_x_deletes_char() {
        let mut mode = VimMode::Normal;
        let mut text = "hello".to_string();
        let mut cursor = 1;
        let mut yank = String::new();
        apply_vim_command(&mut mode, &mut text, &mut cursor, "x", &mut yank);
        assert_eq!(text, "hllo");
        assert_eq!(yank, "e");
    }

    #[test]
    fn vim_dd_clears_text() {
        let mut mode = VimMode::Normal;
        let mut text = "hello world".to_string();
        let mut cursor = 3;
        let mut yank = String::new();
        apply_vim_command(&mut mode, &mut text, &mut cursor, "dd", &mut yank);
        assert!(text.is_empty());
        assert_eq!(cursor, 0);
        assert_eq!(yank, "hello world");
    }

    #[test]
    fn vim_yy_copies_text() {
        let mut mode = VimMode::Normal;
        let mut text = "hello".to_string();
        let mut cursor = 0;
        let mut yank = String::new();
        apply_vim_command(&mut mode, &mut text, &mut cursor, "yy", &mut yank);
        assert_eq!(yank, "hello");
        assert_eq!(text, "hello"); // unchanged
    }

    #[test]
    fn vim_p_pastes_after_cursor() {
        let mut mode = VimMode::Normal;
        let mut text = "ab".to_string();
        let mut cursor = 0;
        let mut yank = "XY".to_string();
        apply_vim_command(&mut mode, &mut text, &mut cursor, "p", &mut yank);
        assert_eq!(text, "aXYb");
    }

    // ---- PromptInputState -----------------------------------------------

    #[test]
    fn insert_char_updates_cursor() {
        let mut s = PromptInputState::new();
        s.insert_char('h');
        s.insert_char('i');
        assert_eq!(s.text, "hi");
        assert_eq!(s.cursor, 2);
    }

    #[test]
    fn insert_newline_works() {
        let mut s = PromptInputState::new();
        s.insert_char('a');
        s.insert_newline();
        s.insert_char('b');
        assert_eq!(s.text, "a\nb");
    }

    #[test]
    fn backspace_removes_previous_char() {
        let mut s = PromptInputState::new();
        s.text = "hello".to_string();
        s.cursor = 5;
        s.backspace();
        assert_eq!(s.text, "hell");
        assert_eq!(s.cursor, 4);
    }

    #[test]
    fn backspace_at_start_is_noop() {
        let mut s = PromptInputState::new();
        s.text = "hi".to_string();
        s.cursor = 0;
        s.backspace();
        assert_eq!(s.text, "hi");
    }

    #[test]
    fn delete_removes_char_at_cursor() {
        let mut s = PromptInputState::new();
        s.text = "hello".to_string();
        s.cursor = 1;
        s.delete();
        assert_eq!(s.text, "hllo");
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn move_left_right() {
        let mut s = PromptInputState::new();
        s.text = "abc".to_string();
        s.cursor = 1;
        s.move_right();
        assert_eq!(s.cursor, 2);
        s.move_left();
        assert_eq!(s.cursor, 1);
    }

    #[test]
    fn cursor_visual_pos_counts_wide_characters() {
        let mut s = PromptInputState::new();
        s.text = "你a".to_string();
        s.cursor = "你".len();

        assert_eq!(s.cursor_visual_pos(10), (0, 2));
    }

    #[test]
    fn render_cursor_after_wide_character() {
        let mut s = PromptInputState::new();
        s.text = "你a".to_string();
        s.cursor = "你".len();

        let area = Rect { x: 0, y: 0, width: 12, height: 4 };
        let mut buf = Buffer::empty(area);
        render_prompt_input(
            &s,
            area,
            &mut buf,
            true,
            InputMode::Default,
            Color::Blue,
            false,
        );

        assert_eq!(buf[(4, 1)].symbol(), "\u{2588}");
    }

    #[test]
    fn readonly_blocks_insert() {
        let mut s = PromptInputState::new();
        s.mode = InputMode::Readonly;
        s.insert_char('x');
        assert!(s.text.is_empty());
    }

    #[test]
    fn history_navigation_up_down() {
        let mut s = PromptInputState::new();
        s.history = vec!["first".to_string(), "second".to_string()];
        s.history_up();
        assert_eq!(s.text, "second");
        s.history_up();
        assert_eq!(s.text, "first");
        s.history_down();
        assert_eq!(s.text, "second");
        s.history_down();
        assert_eq!(s.text, "");
        assert!(s.history_pos.is_none());
    }

    #[test]
    fn history_draft_restored() {
        let mut s = PromptInputState::new();
        s.text = "draft text".to_string();
        s.cursor = 10;
        s.history = vec!["old entry".to_string()];
        s.history_up();
        assert_eq!(s.text, "old entry");
        s.history_down();
        assert_eq!(s.text, "draft text");
    }

    #[test]
    fn clear_resets_state() {
        let mut s = PromptInputState::new();
        s.text = "something".to_string();
        s.cursor = 5;
        s.token_estimate = 10;
        s.clear();
        assert!(s.text.is_empty());
        assert_eq!(s.cursor, 0);
        assert_eq!(s.token_estimate, 0);
    }

    #[test]
    fn take_returns_and_clears() {
        let mut s = PromptInputState::new();
        s.text = "hello".to_string();
        s.cursor = 5;
        let taken = s.take();
        assert_eq!(taken, "hello");
        assert!(s.text.is_empty());
    }

    #[test]
    fn is_empty_trims_whitespace() {
        let mut s = PromptInputState::new();
        s.text = "   \n  ".to_string();
        assert!(s.is_empty());
        s.text = "  x  ".to_string();
        assert!(!s.is_empty());
    }

    // ---- handle_paste ---------------------------------------------------

    #[test]
    fn paste_small_content_inline() {
        let mut counter = 0u32;
        let (result, stored) = handle_paste("short text", &mut counter);
        assert_eq!(result, "short text");
        assert!(stored.is_none());
        assert_eq!(counter, 0);
    }

    #[test]
    fn paste_large_content_placeholder() {
        let mut counter = 0u32;
        // >150 chars, single line → placeholder without a line count
        let big = "x".repeat(200);
        let (result, stored) = handle_paste(&big, &mut counter);
        assert_eq!(result, "[Pasted text #1]");
        assert!(stored.is_some());
        assert_eq!(counter, 1);
    }

    #[test]
    fn paste_large_multiline_placeholder() {
        let mut counter = 0u32;
        // ≥3 lines → triggers placeholder regardless of length
        let big = "line\n".repeat(300);
        let (result, stored) = handle_paste(&big, &mut counter);
        assert_eq!(result, "[Pasted text #1 +300 lines]");
        assert!(stored.is_some());
    }

    #[test]
    fn paste_three_lines_triggers_placeholder() {
        let mut counter = 0u32;
        // Exactly 3 lines (the threshold) should use a placeholder.
        let three_lines = "a\nb\nc";
        let (result, stored) = handle_paste(three_lines, &mut counter);
        assert_eq!(result, "[Pasted text #1 +2 lines]");
        assert!(stored.is_some());
    }

    #[test]
    fn paste_two_lines_inline() {
        let mut counter = 0u32;
        // 2 lines, ≤150 chars → inserted verbatim
        let two_lines = "hello\nworld";
        let (result, stored) = handle_paste(two_lines, &mut counter);
        assert_eq!(result, two_lines);
        assert!(stored.is_none());
    }

    #[test]
    fn paste_counter_increments() {
        let mut counter = 0u32;
        let big = "x".repeat(2000);
        handle_paste(&big, &mut counter);
        handle_paste(&big, &mut counter);
        assert_eq!(counter, 2);
    }

    #[test]
    fn paste_contents_evicted_on_submit() {
        // #223: every large paste stored its body in `paste_contents` but the
        // map was never pruned, so it grew for the App's lifetime. Submitting
        // (take → clear) must reclaim the stored bodies.
        let mut s = PromptInputState::new();
        let block_a = "alpha line\n".repeat(50);
        let block_b = "beta line\n".repeat(60);
        let block_c = "gamma line\n".repeat(70);

        s.paste(&block_a);
        s.paste(" and then "); // small — inlined, no counter bump, not stored
        s.paste(&block_b);
        s.paste(&block_c);

        // Three large pastes were stored, keyed by the incrementing counter.
        assert_eq!(s.paste_contents.len(), 3, "each large paste should be stored");

        // Before eviction every placeholder in the buffer maps back to its
        // original body.
        assert!(s.text.contains("#1") && s.text.contains("#2") && s.text.contains("#3"));
        assert_eq!(s.paste_contents.get(&1), Some(&block_a));
        assert_eq!(s.paste_contents.get(&2), Some(&block_b));
        assert_eq!(s.paste_contents.get(&3), Some(&block_c));

        // Submit. The taken text carries the expanded bodies (the agent must
        // receive the real content, not the placeholders) and the stored
        // bodies are reclaimed.
        let taken = s.take();
        assert!(!taken.contains("[Pasted text #"), "placeholders must be expanded on take");
        assert!(taken.contains(&block_a) && taken.contains(&block_b) && taken.contains(&block_c));
        assert!(taken.contains(" and then "), "inline text between pastes survives");
        assert!(s.paste_contents.is_empty(), "paste_contents must be emptied on submit (#223)");
        assert!(s.text.is_empty());

        // A fresh paste after submit starts clean and bounded.
        s.paste(&block_a);
        assert_eq!(s.paste_contents.len(), 1);
    }

    #[test]
    fn paste_contents_evicted_on_cancel() {
        // Discarding the buffer (Esc / clear) must also reclaim stored pastes.
        let mut s = PromptInputState::new();
        s.paste(&"discard me\n".repeat(40));
        assert_eq!(s.paste_contents.len(), 1);
        s.clear();
        assert!(s.paste_contents.is_empty(), "clear() must reclaim stored pastes (#223)");
    }

    #[test]
    fn take_expands_placeholder_with_surrounding_text() {
        let mut s = PromptInputState::new();
        for c in "explain this: ".chars() {
            s.insert_char(c);
        }
        let body = "fn main() {\n    println!(\"hi\");\n}\n";
        s.paste(body);
        for c in " please".chars() {
            s.insert_char(c);
        }
        assert!(s.text.contains("[Pasted text #1"));
        let taken = s.take();
        assert_eq!(taken, format!("explain this: {body} please"));
    }

    #[test]
    fn take_leaves_unknown_refs_alone() {
        // A hand-typed reference with no stored body must pass through
        // untouched instead of vanishing or panicking.
        let mut s = PromptInputState::new();
        for c in "see [Pasted text #7] there".chars() {
            s.insert_char(c);
        }
        assert_eq!(s.take(), "see [Pasted text #7] there");
    }

    #[test]
    fn take_expands_crlf_paste_normalized() {
        // CRLF pastes are normalized at store time, so the expanded submit
        // text never re-introduces a bare '\r' (#221 invariant).
        let mut s = PromptInputState::new();
        s.paste("a\r\nb\r\nc\r\nd");
        let taken = s.take();
        assert_eq!(taken, "a\nb\nc\nd");
    }

    #[test]
    fn expand_paste_ref_at_cursor_inline() {
        let mut s = PromptInputState::new();
        let body = "x\ny\nz";
        s.paste(body);
        // Cursor sits right after the placeholder post-paste (inclusive end).
        assert!(s.expand_paste_ref_at_cursor());
        assert_eq!(s.text, body);
        assert_eq!(s.cursor, body.len());
        assert!(s.paste_contents.is_empty(), "expanded body is dropped from the store");
        // A second expand has nothing left to do.
        assert!(!s.expand_paste_ref_at_cursor());
    }

    #[test]
    fn expand_paste_ref_falls_back_to_first_ref() {
        let mut s = PromptInputState::new();
        s.paste("one\ntwo\nthree");
        // Move the cursor away from the placeholder.
        s.cursor = 0;
        for c in "prefix ".chars() {
            s.insert_char(c);
        }
        s.cursor = 0;
        assert!(s.expand_paste_ref_at_cursor());
        assert_eq!(s.text, "prefix one\ntwo\nthree");
    }

    #[test]
    fn expand_paste_ref_ignores_image_refs() {
        let mut s = PromptInputState::new();
        s.paste("a\nb\nc"); // stored as text paste #1
        s.cursor = 0;
        for c in "[Image #1] ".chars() {
            s.insert_char(c);
        }
        // Cursor inside the image ref: must not splice the text body there.
        s.cursor = 3;
        assert!(!s.expand_paste_ref_at(3));
        assert!(s.text.starts_with("[Image #1] "));
        // The text placeholder itself still expands.
        assert!(s.has_expandable_paste_ref());
        assert!(s.expand_paste_ref_at(s.text.len()));
        assert_eq!(s.text, "[Image #1] a\nb\nc");
    }

    // ---- compute_typeahead ---------------------------------------------

    // Helper constants for tests
    const TEST_FILE_AUTOCOMPLETE_LIMIT: usize = 15;
    const TEST_FILE_AUTOCOMPLETE_SHOW_HIDDEN: bool = false;

    #[test]
    fn typeahead_slash_prefix_matches() {
        let cmds = [("help", "Show help"), ("history", "Show history"), ("compact", "Compact")];
        let suggestions = compute_slash_suggestions("/h", &cmds);
        assert_eq!(suggestions.len(), 2);
        assert_eq!(suggestions[0].text, "/help");
        assert_eq!(suggestions[1].text, "/history");
    }

    #[test]
    fn typeahead_full_match() {
        let cmds = [("compact", "Compact conversation")];
        let suggestions = compute_slash_suggestions("/compact", &cmds);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].text, "/compact");
        assert_eq!(suggestions[0].description, "Compact conversation");
    }

    #[test]
    fn typeahead_case_insensitive() {
        let cmds = [("Help", "Show help")];
        let suggestions = compute_slash_suggestions("/H", &cmds);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].text, "/Help");
    }

    // ---- suggestion navigation -----------------------------------------

    #[test]
    fn suggestion_next_cycles() {
        let mut s = PromptInputState::new();
        let cmds = [("help", "Help"), ("history", "History"), ("compact", "Compact")];
        s.text = "/h".to_string();
        s.cursor = s.text.len();
        s.update_suggestions(&cmds, 15, false);
        assert_eq!(s.suggestions.len(), 2);
        assert_eq!(s.suggestion_index, Some(0));
        s.suggestion_next();
        assert_eq!(s.suggestion_index, Some(1));
        s.suggestion_next();
        assert_eq!(s.suggestion_index, Some(0)); // wraps
    }

    #[test]
    fn accept_suggestion_fills_text() {
        let mut s = PromptInputState::new();
        let cmds = [("help", "Show help")];
        s.text = "/he".to_string();
        s.cursor = s.text.len();
        s.update_suggestions(&cmds, 15, false);
        s.suggestion_next();
        s.accept_suggestion();
        assert_eq!(s.text, "/help");
        assert_eq!(s.cursor, 5);
        assert!(s.suggestions.is_empty());
    }

    // ---- token estimate -------------------------------------------------

    #[test]
    fn token_estimate_rough() {
        let mut s = PromptInputState::new();
        for _ in 0..40 {
            s.insert_char('a');
        }
        // 40 chars / 4 = 10 tokens
        assert_eq!(s.token_estimate, 10);
    }

    // ---- motion_w / motion_b -----------------------------------------------

    #[test]
    fn motion_w_basic() {
        assert_eq!(motion_w("hello world", 0), 6);
        assert_eq!(motion_w("hello world", 6), 11); // at start of 'world', moves to end
        assert_eq!(motion_w("  foo", 0), 2);         // skip leading spaces
    }

    #[test]
    fn motion_b_basic() {
        assert_eq!(motion_b("hello world", 6), 0); // 'w' → start of 'hello'
        assert_eq!(motion_b("hello world", 0), 0); // already at start
    }

    #[test]
    fn motion_e_basic() {
        assert_eq!(motion_e("hello world", 0), 4);  // cursor on 'h', end at 'o'
        assert_eq!(motion_e("hello world", 4), 10); // at 'o' (end), jump to 'd'
    }

    #[test]
    fn motion_W_B_basic() {
        // "foo.bar baz"  W from 0 → 8 ('b' of 'baz')
        assert_eq!(motion_W("foo.bar baz", 0), 8);
        assert_eq!(motion_B("foo.bar baz", 8), 0);
    }

    #[test]
    fn motion_E_basic() {
        assert_eq!(motion_E("foo.bar baz", 0), 6); // end of 'foo.bar' WORD
    }

    #[test]
    fn motion_first_nonblank_basic() {
        assert_eq!(motion_first_nonblank("  hello", 0), 2);
        assert_eq!(motion_first_nonblank("hello", 0), 0);
    }

    #[test]
    fn motion_G_basic() {
        assert_eq!(motion_G("foo\nbar"), 4);
        assert_eq!(motion_G("single line"), 0);
    }

    #[test]
    fn motion_gg_basic() {
        assert_eq!(motion_gg("foo\nbar\nbaz", 1), 0);
        assert_eq!(motion_gg("foo\nbar\nbaz", 2), 4);
        assert_eq!(motion_gg("foo\nbar\nbaz", 3), 8);
    }

    #[test]
    fn motion_find_char_f() {
        // f: cursor lands on 'o', count=1
        assert_eq!(motion_find_char("hello", 0, 'o', VimFindKind::F, 1), Some(4));
        // f: not found
        assert_eq!(motion_find_char("hello", 0, 'z', VimFindKind::F, 1), None);
    }

    #[test]
    fn motion_find_char_t() {
        // t: cursor stops before 'o'
        assert_eq!(motion_find_char("hello", 0, 'o', VimFindKind::T, 1), Some(3));
    }

    #[test]
    fn motion_find_char_bigF() {
        // F: search backward
        assert_eq!(motion_find_char("hello", 4, 'h', VimFindKind::BigF, 1), Some(0));
    }

    // ---- apply_vim_key new commands ----------------------------------------

    #[test]
    fn vim_key_e_motion() {
        let mut mode = VimMode::Normal;
        let mut text = "hello world".to_string();
        let mut cursor = 0usize;
        let mut yank = String::new();
        let mut pending = VimPendingState::None;
        let mut last_find = None;
        apply_vim_key(&mut mode, &mut text, &mut cursor, "e", &mut yank, &mut pending, &mut last_find);
        assert_eq!(cursor, 4); // end of 'hello'
    }

    #[test]
    fn vim_key_W_motion() {
        let mut mode = VimMode::Normal;
        let mut text = "foo.bar baz".to_string();
        let mut cursor = 0usize;
        let mut yank = String::new();
        let mut pending = VimPendingState::None;
        let mut last_find = None;
        apply_vim_key(&mut mode, &mut text, &mut cursor, "W", &mut yank, &mut pending, &mut last_find);
        assert_eq!(cursor, 8); // 'baz'
    }

    #[test]
    fn vim_key_G_last_line() {
        let mut mode = VimMode::Normal;
        let mut text = "first\nsecond\nthird".to_string();
        let mut cursor = 0usize;
        let mut yank = String::new();
        let mut pending = VimPendingState::None;
        let mut last_find = None;
        apply_vim_key(&mut mode, &mut text, &mut cursor, "G", &mut yank, &mut pending, &mut last_find);
        assert_eq!(cursor, 13); // start of 'third'
    }

    #[test]
    fn vim_key_gg_first_line() {
        let mut mode = VimMode::Normal;
        let mut text = "first\nsecond".to_string();
        let mut cursor = 6usize;
        let mut yank = String::new();
        let mut pending = VimPendingState::None;
        let mut last_find = None;
        // 'g' sets pending G
        apply_vim_key(&mut mode, &mut text, &mut cursor, "g", &mut yank, &mut pending, &mut last_find);
        assert!(matches!(pending, VimPendingState::G { .. }));
        apply_vim_key(&mut mode, &mut text, &mut cursor, "g", &mut yank, &mut pending, &mut last_find);
        assert_eq!(cursor, 0);
    }

    #[test]
    fn vim_key_count_motion() {
        let mut mode = VimMode::Normal;
        let mut text = "a b c d e".to_string();
        let mut cursor = 0usize;
        let mut yank = String::new();
        let mut pending = VimPendingState::None;
        let mut last_find = None;
        // 3w — advance 3 words
        apply_vim_key(&mut mode, &mut text, &mut cursor, "3", &mut yank, &mut pending, &mut last_find);
        assert!(matches!(pending, VimPendingState::Count { .. }));
        apply_vim_key(&mut mode, &mut text, &mut cursor, "w", &mut yank, &mut pending, &mut last_find);
        assert_eq!(cursor, 6); // 3 words forward: a→b→c→d start = pos 6
    }

    #[test]
    fn vim_key_dw_delete_word() {
        let mut mode = VimMode::Normal;
        let mut text = "hello world".to_string();
        let mut cursor = 0usize;
        let mut yank = String::new();
        let mut pending = VimPendingState::None;
        let mut last_find = None;
        apply_vim_key(&mut mode, &mut text, &mut cursor, "d", &mut yank, &mut pending, &mut last_find);
        assert!(matches!(pending, VimPendingState::Operator { op: VimOperator::Delete, .. }));
        apply_vim_key(&mut mode, &mut text, &mut cursor, "w", &mut yank, &mut pending, &mut last_find);
        assert_eq!(text, "world");
        assert_eq!(yank, "hello ");
    }

    #[test]
    fn vim_key_cw_change_word_enters_insert() {
        let mut mode = VimMode::Normal;
        let mut text = "hello world".to_string();
        let mut cursor = 0usize;
        let mut yank = String::new();
        let mut pending = VimPendingState::None;
        let mut last_find = None;
        apply_vim_key(&mut mode, &mut text, &mut cursor, "c", &mut yank, &mut pending, &mut last_find);
        apply_vim_key(&mut mode, &mut text, &mut cursor, "w", &mut yank, &mut pending, &mut last_find);
        assert_eq!(mode, VimMode::Insert);
        assert_eq!(text, "world");
    }

    #[test]
    fn vim_key_dd_deletes_line() {
        let mut mode = VimMode::Normal;
        let mut text = "first\nsecond".to_string();
        let mut cursor = 0usize;
        let mut yank = String::new();
        let mut pending = VimPendingState::None;
        let mut last_find = None;
        apply_vim_key(&mut mode, &mut text, &mut cursor, "d", &mut yank, &mut pending, &mut last_find);
        apply_vim_key(&mut mode, &mut text, &mut cursor, "d", &mut yank, &mut pending, &mut last_find);
        assert_eq!(text, "second");
        assert_eq!(yank, "first\n");
    }

    #[test]
    fn vim_key_r_replace_char() {
        let mut mode = VimMode::Normal;
        let mut text = "hello".to_string();
        let mut cursor = 0usize;
        let mut yank = String::new();
        let mut pending = VimPendingState::None;
        let mut last_find = None;
        apply_vim_key(&mut mode, &mut text, &mut cursor, "r", &mut yank, &mut pending, &mut last_find);
        assert!(matches!(pending, VimPendingState::Replace { .. }));
        apply_vim_key(&mut mode, &mut text, &mut cursor, "H", &mut yank, &mut pending, &mut last_find);
        assert_eq!(text, "Hello");
        assert_eq!(mode, VimMode::Normal); // stays in Normal after replace
    }

    #[test]
    fn vim_key_find_f() {
        let mut mode = VimMode::Normal;
        let mut text = "hello world".to_string();
        let mut cursor = 0usize;
        let mut yank = String::new();
        let mut pending = VimPendingState::None;
        let mut last_find = None;
        apply_vim_key(&mut mode, &mut text, &mut cursor, "f", &mut yank, &mut pending, &mut last_find);
        apply_vim_key(&mut mode, &mut text, &mut cursor, "o", &mut yank, &mut pending, &mut last_find);
        assert_eq!(cursor, 4); // first 'o' in 'hello'
        assert_eq!(last_find, Some((VimFindKind::F, 'o')));
    }

    #[test]
    fn vim_key_semicolon_repeat_find() {
        let mut mode = VimMode::Normal;
        let mut text = "a.b.c".to_string();
        let mut cursor = 0usize;
        let mut yank = String::new();
        let mut pending = VimPendingState::None;
        let mut last_find = None;
        apply_vim_key(&mut mode, &mut text, &mut cursor, "f", &mut yank, &mut pending, &mut last_find);
        apply_vim_key(&mut mode, &mut text, &mut cursor, ".", &mut yank, &mut pending, &mut last_find);
        assert_eq!(cursor, 1);
        apply_vim_key(&mut mode, &mut text, &mut cursor, ";", &mut yank, &mut pending, &mut last_find);
        assert_eq!(cursor, 3); // repeated find → next '.'
    }

    #[test]
    fn vim_key_X_delete_before_cursor() {
        let mut mode = VimMode::Normal;
        let mut text = "hello".to_string();
        let mut cursor = 4usize;
        let mut yank = String::new();
        let mut pending = VimPendingState::None;
        let mut last_find = None;
        apply_vim_key(&mut mode, &mut text, &mut cursor, "X", &mut yank, &mut pending, &mut last_find);
        assert_eq!(text, "helo");
        assert_eq!(cursor, 3);
    }

    #[test]
    fn vim_key_tilde_toggle_case() {
        let mut mode = VimMode::Normal;
        let mut text = "hello".to_string();
        let mut cursor = 0usize;
        let mut yank = String::new();
        let mut pending = VimPendingState::None;
        let mut last_find = None;
        apply_vim_key(&mut mode, &mut text, &mut cursor, "~", &mut yank, &mut pending, &mut last_find);
        assert_eq!(text, "Hello");
    }

    #[test]
    fn vim_key_o_open_line_below() {
        let mut mode = VimMode::Normal;
        let mut text = "first\nthird".to_string();
        let mut cursor = 0usize;
        let mut yank = String::new();
        let mut pending = VimPendingState::None;
        let mut last_find = None;
        apply_vim_key(&mut mode, &mut text, &mut cursor, "o", &mut yank, &mut pending, &mut last_find);
        assert_eq!(mode, VimMode::Insert);
        assert!(text.contains('\n'));
        assert_eq!(cursor, 6); // after first newline
    }

    #[test]
    fn vim_key_D_delete_to_eol() {
        let mut mode = VimMode::Normal;
        let mut text = "hello world".to_string();
        let mut cursor = 6usize;
        let mut yank = String::new();
        let mut pending = VimPendingState::None;
        let mut last_find = None;
        apply_vim_key(&mut mode, &mut text, &mut cursor, "D", &mut yank, &mut pending, &mut last_find);
        assert_eq!(text, "hello ");
        assert_eq!(yank, "world");
    }

    // ---- PromptInputState undo ---------------------------------------------

    #[test]
    fn prompt_input_undo_restores_text() {
        let mut s = PromptInputState::new();
        s.vim_enabled = true;
        s.vim_mode = VimMode::Normal;
        s.text = "hello".to_string();
        s.cursor = 5;
        s.vim_command("x"); // deletes 'o' (but cursor at 5 = past end)
        // let's set cursor to 4 and delete
        s.cursor = 4;
        s.vim_command("x");
        assert_eq!(s.text, "hell");
        s.vim_command("u");
        assert_eq!(s.text, "hello");
    }

    #[test]
    fn prompt_input_visual_yank() {
        let mut s = PromptInputState::new();
        s.vim_enabled = true;
        s.vim_mode = VimMode::Normal;
        s.text = "hello world".to_string();
        s.cursor = 0;
        s.vim_command("v");
        assert_eq!(s.vim_mode, VimMode::Visual);
        // Move to end of word
        s.vim_command("e");
        s.vim_command("y"); // yank selection
        assert_eq!(s.yank_buf, "hello");
        assert_eq!(s.vim_mode, VimMode::Normal);
    }

    // ---- Named registers ------------------------------------------------

    #[test]
    fn register_yank_and_paste() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.text = "hello world".to_string();
        s.cursor = 0;
        // `"ay` — yank line to register 'a'
        s.vim_command("\"");
        s.vim_command("a");
        s.vim_command("y");
        assert_eq!(s.vim_registers.get(&'a').map(|s| s.as_str()), Some("hello world"));
        // `"ap` — paste from register 'a' after cursor
        s.cursor = 0;
        s.vim_command("\"");
        s.vim_command("a");
        s.vim_command("p");
        assert!(s.text.contains("hello world"));
    }

    #[test]
    fn register_yank_method() {
        let mut s = PromptInputState::new();
        s.yank_to_register('b', "some text");
        assert_eq!(s.paste_from_register('b'), Some("some text".to_string()));
        assert_eq!(s.paste_from_register('z'), None);
    }

    #[test]
    fn register_delete_to_named() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.text = "hello\nworld".to_string();
        s.cursor = 0;
        // `"ad` — delete line to register 'a'
        s.vim_command("\"");
        s.vim_command("a");
        s.vim_command("d");
        assert_eq!(s.vim_registers.get(&'a').map(|s| s.as_str()), Some("hello\n"));
        assert_eq!(s.text, "world");
    }

    // ---- Marks ----------------------------------------------------------

    #[test]
    fn mark_set_and_jump() {
        let mut s = PromptInputState::new();
        s.text = "hello world".to_string();
        s.cursor = 6; // at 'w'
        s.set_mark('a');
        s.cursor = 0;
        s.jump_to_mark('a');
        assert_eq!(s.cursor, 6);
    }

    #[test]
    fn mark_jump_nonexistent_is_noop() {
        let mut s = PromptInputState::new();
        s.text = "hello".to_string();
        s.cursor = 3;
        s.jump_to_mark('z'); // no mark 'z' set
        assert_eq!(s.cursor, 3);
    }

    #[test]
    fn mark_via_vim_command() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.text = "hello world".to_string();
        s.cursor = 6;
        // `ma` — set mark 'a'
        s.vim_command("m");
        s.vim_command("a");
        assert!(s.vim_marks.contains_key(&'a'));
        // Move cursor and jump back with `'a`
        s.cursor = 0;
        s.vim_command("'");
        s.vim_command("a");
        assert_eq!(s.cursor, 6);
    }

    #[test]
    fn mark_clamped_when_text_shortened() {
        let mut s = PromptInputState::new();
        s.text = "hello world".to_string();
        s.cursor = 10;
        s.set_mark('x');
        // Shorten the text
        s.text = "hi".to_string();
        s.cursor = 0;
        s.jump_to_mark('x');
        // Should clamp to text length
        assert!(s.cursor <= s.text.len());
        assert!(s.text.is_char_boundary(s.cursor));
    }

    // ---- Macro recording ------------------------------------------------

    #[test]
    fn macro_record_and_replay() {
        let mut s = PromptInputState::new();
        // Start recording into register 'q'
        s.start_macro_recording('q');
        assert_eq!(s.vim_macro_recording, Some('q'));
        // Simulate accumulating keys
        s.vim_macro_content.get_mut(&'q').unwrap().push("w".to_string());
        s.vim_macro_content.get_mut(&'q').unwrap().push("e".to_string());
        // Stop recording
        let reg = s.stop_macro_recording();
        assert_eq!(reg, Some('q'));
        assert_eq!(s.vim_macro_recording, None);
        // Replay
        let keys = s.replay_macro('q');
        assert_eq!(keys, vec!["w".to_string(), "e".to_string()]);
    }

    #[test]
    fn macro_replay_empty_register() {
        let s = PromptInputState::new();
        let keys = s.replay_macro('z');
        assert!(keys.is_empty());
    }

    #[test]
    fn macro_via_vim_command() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.text = "abc".to_string();
        s.cursor = 0;
        // `qq` — start recording into 'q'
        s.vim_command("q");
        assert!(matches!(s.vim_pending, VimPendingState::MacroRecord));
        s.vim_command("q"); // register name = 'q'
        assert_eq!(s.vim_macro_recording, Some('q'));
        // Record some keys: move right twice
        s.vim_command("l");
        s.vim_command("l");
        // Stop recording with `q`
        s.vim_command("q");
        assert_eq!(s.vim_macro_recording, None);
        // The recorded content should have 'l', 'l'
        let keys = s.replay_macro('q');
        assert_eq!(keys, vec!["l".to_string(), "l".to_string()]);
    }

    #[test]
    fn macro_replay_via_at() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.text = "abcdef".to_string();
        s.cursor = 0;
        // Manually record a macro: move 2 chars right
        s.vim_macro_content.insert('q', vec!["l".to_string(), "l".to_string()]);
        // `@q` — replay macro 'q'
        s.vim_command("@");
        assert!(matches!(s.vim_pending, VimPendingState::MacroReplay));
        s.vim_command("q");
        // cursor should have moved right by 2
        assert_eq!(s.cursor, 2);
    }

    // ---- Dot-repeat -----------------------------------------------------

    #[test]
    fn dot_repeat_delete_char() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.text = "hello".to_string();
        s.cursor = 0;
        // Delete char at cursor with `x`
        s.vim_command("x");
        assert_eq!(s.text, "ello");
        // Dot-repeat should delete again
        s.vim_command(".");
        assert_eq!(s.text, "llo");
    }

    #[test]
    fn dot_repeat_replace_char() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.text = "hello".to_string();
        s.cursor = 0;
        // Replace 'h' with 'H' using `r`
        s.vim_command("r");
        s.vim_command("H");
        assert_eq!(s.text, "Hello");
        // Move and dot-repeat: should replace 'e' with 'H'
        s.vim_command("l");
        s.vim_command(".");
        assert_eq!(s.text, "HHllo");
    }

    #[test]
    fn dot_repeat_noop_when_no_action() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.text = "hello".to_string();
        s.cursor = 0;
        // `.` with no prior modifying action should be a no-op
        s.vim_command(".");
        assert_eq!(s.text, "hello");
        assert_eq!(s.cursor, 0);
    }

    #[test]
    fn dot_repeat_after_visual_delete() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.text = "hello world".to_string();
        s.cursor = 0;
        // Enter visual, select 'hel', then delete
        s.vim_command("v");
        s.vim_command("l");
        s.vim_command("l");
        s.vim_command("d");
        assert_eq!(s.text, "lo world");
        // Dot-repeat should delete chars again
        s.vim_command(".");
        // The text should be shorter
        assert!(s.text.len() < "lo world".len());
    }

    // ---- Visual line mode (V) -------------------------------------------

    #[test]
    fn visual_line_mode_enter() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.text = "line one\nline two".to_string();
        s.cursor = 0;
        s.vim_command("V");
        assert_eq!(s.vim_mode, VimMode::VisualLine);
        assert!(s.visual_anchor.is_some());
    }

    #[test]
    fn visual_line_yank() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.text = "line one\nline two".to_string();
        s.cursor = 0;
        s.vim_command("V");
        s.vim_command("y");
        assert_eq!(s.vim_mode, VimMode::Normal);
        assert_eq!(s.yank_buf, "line one\n");
    }

    #[test]
    fn visual_line_delete() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.text = "line one\nline two".to_string();
        s.cursor = 0;
        s.vim_command("V");
        s.vim_command("d");
        assert_eq!(s.vim_mode, VimMode::Normal);
        assert_eq!(s.text, "line two");
    }

    #[test]
    fn visual_line_escape_returns_normal() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.text = "hello".to_string();
        s.vim_command("V");
        assert_eq!(s.vim_mode, VimMode::VisualLine);
        s.vim_command("Escape");
        assert_eq!(s.vim_mode, VimMode::Normal);
    }

    // ---- Command-line mode (:) ------------------------------------------

    #[test]
    fn command_line_mode_enter() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.vim_command(":");
        assert_eq!(s.vim_mode, VimMode::Command);
        assert!(s.vim_command_buf.is_empty());
    }

    #[test]
    fn command_line_accumulates_chars() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.vim_command(":");
        s.vim_command("q");
        assert_eq!(s.vim_command_buf, "q");
        s.vim_command("!");
        assert_eq!(s.vim_command_buf, "q!");
    }

    #[test]
    fn command_line_backspace_pops() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.vim_command(":");
        s.vim_command("q");
        s.vim_command("w");
        s.vim_command("Backspace");
        assert_eq!(s.vim_command_buf, "q");
    }

    #[test]
    fn command_line_empty_backspace_cancels() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.vim_command(":");
        s.vim_command("Backspace");
        assert_eq!(s.vim_mode, VimMode::Normal);
    }

    #[test]
    fn command_q_sets_quit_flag() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.vim_command(":");
        s.vim_command("q");
        s.vim_command("Enter");
        assert!(s.vim_quit_requested);
        assert_eq!(s.vim_mode, VimMode::Normal);
    }

    #[test]
    fn command_noh_clears_search() {
        let mut s = PromptInputState::new();
        s.vim_search_last = Some("foo".to_string());
        s.vim_mode = VimMode::Normal;
        s.vim_command(":");
        for c in "noh".chars() {
            s.vim_command(&c.to_string());
        }
        s.vim_command("Enter");
        assert!(s.vim_search_last.is_none());
    }

    #[test]
    fn command_escape_cancels() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.vim_command(":");
        s.vim_command("q");
        s.vim_command("Escape");
        assert_eq!(s.vim_mode, VimMode::Normal);
    }

    // ---- In-prompt search (/) -------------------------------------------

    #[test]
    fn search_mode_enter() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.vim_command("/");
        assert_eq!(s.vim_mode, VimMode::Search);
        assert!(s.vim_search_buf.is_empty());
    }

    #[test]
    fn search_finds_match_and_moves_cursor() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.text = "hello world hello".to_string();
        s.cursor = 0;
        s.vim_command("/");
        for c in "world".chars() {
            s.vim_command(&c.to_string());
        }
        s.vim_command("Enter");
        assert_eq!(s.vim_mode, VimMode::Normal);
        assert_eq!(s.cursor, 6); // "world" starts at byte 6
        assert_eq!(s.vim_search_last.as_deref(), Some("world"));
    }

    #[test]
    fn search_n_finds_next() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.text = "aa bb aa".to_string();
        s.cursor = 0;
        s.vim_command("/");
        s.vim_command("a");
        s.vim_command("a");
        s.vim_command("Enter");
        assert_eq!(s.cursor, 0); // first 'aa'
        s.vim_command("n");
        assert_eq!(s.cursor, 6); // second 'aa'
    }

    #[test]
    fn search_N_finds_prev() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.text = "aa bb aa".to_string();
        s.cursor = 7; // at second 'aa'
        s.vim_search_last = Some("aa".to_string());
        s.vim_command("N");
        assert_eq!(s.cursor, 0); // wraps to first 'aa'
    }

    #[test]
    fn search_escape_cancels() {
        let mut s = PromptInputState::new();
        s.vim_mode = VimMode::Normal;
        s.vim_command("/");
        s.vim_command("f");
        s.vim_command("Escape");
        assert_eq!(s.vim_mode, VimMode::Normal);
    }

    // ---- VimMode labels -------------------------------------------------

    #[test]
    fn vim_mode_new_labels() {
        assert_eq!(VimMode::VisualLine.label(), "VISUAL LINE");
        assert_eq!(VimMode::Command.label(), "COMMAND");
        assert_eq!(VimMode::Search.label(), "SEARCH");
    }

    // ---- File reference (@) autocomplete tests ----

    #[test]
    fn file_autocomplete_slash_commands_still_work() {
        let cmds = vec![("help", "Show help"), ("clear", "Clear messages")];
        let suggestions = compute_slash_suggestions("/he", &cmds);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].text, "/help");
    }

    #[test]
    fn file_autocomplete_at_requires_word_boundary() {
        // @ at word boundary: should suggest files (or be empty if cwd has no files)
        let suggestions_at_boundary = compute_file_suggestions("@", TEST_FILE_AUTOCOMPLETE_LIMIT, TEST_FILE_AUTOCOMPLETE_SHOW_HIDDEN);
        let suggestions_at_boundary_with_space = compute_file_suggestions("hello @", TEST_FILE_AUTOCOMPLETE_LIMIT, TEST_FILE_AUTOCOMPLETE_SHOW_HIDDEN);

        // @ not at word boundary: should never suggest files
        let suggestions_no_boundary = compute_file_suggestions("test@", TEST_FILE_AUTOCOMPLETE_LIMIT, TEST_FILE_AUTOCOMPLETE_SHOW_HIDDEN);
        assert!(suggestions_no_boundary.is_empty(), "@ without word boundary should never suggest files");

        // At least one of the boundary cases should work if cwd has files
        // but more importantly, the non-boundary case should always be empty
        for suggestion in suggestions_at_boundary.iter().chain(suggestions_at_boundary_with_space.iter()) {
            assert_eq!(suggestion.source, TypeaheadSource::FileRef);
        }
    }

    #[test]
    fn file_autocomplete_returns_fileref_source() {
        let suggestions = compute_file_suggestions("@", TEST_FILE_AUTOCOMPLETE_LIMIT, TEST_FILE_AUTOCOMPLETE_SHOW_HIDDEN);

        for suggestion in suggestions {
            assert_eq!(suggestion.source, TypeaheadSource::FileRef);
        }
    }

    #[test]
    fn file_autocomplete_format_filenames() {
        let suggestions = compute_file_suggestions("@", TEST_FILE_AUTOCOMPLETE_LIMIT, TEST_FILE_AUTOCOMPLETE_SHOW_HIDDEN);

        // All suggestions should start with @
        for suggestion in suggestions {
            assert!(suggestion.text.starts_with('@'));
        }
    }

    #[test]
    fn file_autocomplete_with_whitespace_prefix() {
        // @ after whitespace: should suggest files
        let suggestions = compute_file_suggestions("hello @", TEST_FILE_AUTOCOMPLETE_LIMIT, TEST_FILE_AUTOCOMPLETE_SHOW_HIDDEN);

        // Check they all start with @ and are FileRef source
        for suggestion in suggestions {
            assert!(suggestion.text.starts_with('@'));
            assert_eq!(suggestion.source, TypeaheadSource::FileRef);
        }
    }

    #[test]
    fn file_autocomplete_detects_symlinks() {
        // This test verifies that symlinks/junction links are properly detected.
        // On systems with symlinks/junctions, suggestions will include descriptions
        // like "file link" or "directory link".
        let suggestions = compute_file_suggestions("@", TEST_FILE_AUTOCOMPLETE_LIMIT, TEST_FILE_AUTOCOMPLETE_SHOW_HIDDEN);

        // All suggestions should have a description (file, directory, file link, or directory link)
        for suggestion in suggestions {
            assert!(!suggestion.description.is_empty());
            assert!(
                suggestion.description.contains("file")
                    || suggestion.description.contains("directory"),
                "Unexpected description: {}",
                suggestion.description
            );
        }
    }

    // ---- has_active_file_ref tests ----------------------------------------

    #[test]
    fn has_active_file_ref_at_start() {
        let mut s = PromptInputState::new();
        s.text = "@src/".to_string();
        s.cursor = s.text.len();
        assert!(s.has_active_file_ref());
    }

    #[test]
    fn has_active_file_ref_after_space() {
        let mut s = PromptInputState::new();
        s.text = "hello @".to_string();
        s.cursor = s.text.len();
        assert!(s.has_active_file_ref());
    }

    #[test]
    fn has_active_file_ref_email_not_boundary() {
        let mut s = PromptInputState::new();
        s.text = "email@host".to_string();
        s.cursor = s.text.len();
        assert!(!s.has_active_file_ref());
    }

    #[test]
    fn has_active_file_ref_no_at() {
        let mut s = PromptInputState::new();
        s.text = "no at sign here".to_string();
        s.cursor = s.text.len();
        assert!(!s.has_active_file_ref());
    }

    // ---- accept_suggestion FileRef tests ------------------------------------

    #[test]
    fn accept_suggestion_file_ref_at_start() {
        let mut s = PromptInputState::new();
        s.text = "@src/ma".to_string();
        s.cursor = s.text.len();
        s.suggestions = vec![TypeaheadSuggestion {
            text: "@src/main.rs".to_string(),
            description: "file".to_string(),
            source: TypeaheadSource::FileRef,
        }];
        s.suggestion_index = Some(0);
        s.accept_suggestion();
        assert_eq!(s.text, "@src/main.rs");
        assert_eq!(s.cursor, "@src/main.rs".len());
        assert!(s.suggestions.is_empty());
    }

    #[test]
    fn accept_suggestion_file_ref_after_text_preserves_prefix() {
        let mut s = PromptInputState::new();
        s.text = "some text @src/ma".to_string();
        s.cursor = s.text.len();
        s.suggestions = vec![TypeaheadSuggestion {
            text: "@src/main.rs".to_string(),
            description: "file".to_string(),
            source: TypeaheadSource::FileRef,
        }];
        s.suggestion_index = Some(0);
        s.accept_suggestion();
        assert_eq!(s.text, "some text @src/main.rs");
        assert_eq!(s.cursor, "some text @src/main.rs".len());
    }

    #[test]
    fn accept_suggestion_file_ref_preserves_tail() {
        let mut s = PromptInputState::new();
        // Cursor is mid-string; tail after cursor is preserved
        let prefix = "@src/ma";
        let tail = " more text";
        s.text = format!("{}{}", prefix, tail);
        s.cursor = prefix.len();
        s.suggestions = vec![TypeaheadSuggestion {
            text: "@src/main.rs".to_string(),
            description: "file".to_string(),
            source: TypeaheadSource::FileRef,
        }];
        s.suggestion_index = Some(0);
        s.accept_suggestion();
        assert_eq!(s.text, "@src/main.rs more text");
        assert_eq!(s.cursor, "@src/main.rs".len());
    }

    // ---- #221: CRLF + multibyte paste/render safety --------------------

    #[test]
    fn paste_normalizes_crlf_and_lone_cr() {
        // A stray '\r' must never enter the buffer: it desyncs the renderer's
        // cursor byte accounting (which assumes a 1-byte separator) (#221).
        let mut s = PromptInputState::new();
        s.paste("x\r\né");
        assert_eq!(s.text, "x\né");
        assert!(!s.text.contains('\r'));
        assert!(s.text.is_char_boundary(s.cursor));

        let mut s2 = PromptInputState::new();
        s2.paste("a\rb");
        assert_eq!(s2.text, "a\nb");
    }

    #[test]
    fn render_crlf_multibyte_cursor_midcodepoint_no_panic() {
        // Seed a buffer with CRLF + a 2-byte char and place the cursor where the
        // pre-fix accounting mapped mid-codepoint (`&"é"[..1]`) and panicked.
        let mut s = PromptInputState::new();
        s.text = "x\r\né".to_string();
        s.cursor = 3; // byte offset of the start of 'é' (a valid boundary)

        let area = Rect { x: 0, y: 0, width: 12, height: 4 };
        let mut buf = Buffer::empty(area);
        // Reaching the end of this call without panicking is the assertion.
        render_prompt_input(&s, area, &mut buf, true, InputMode::Default, Color::Blue, false);
    }

    // ---- keyword gradient (ultracode + personas) -----------------------

    /// Count of bold Rgb spans and the distinct colors seen in `spans`.
    fn bold_rgb_stats(spans: &[Span<'static>]) -> (usize, std::collections::HashSet<(u8, u8, u8)>) {
        let mut bold_rgb = 0usize;
        let mut colors = std::collections::HashSet::new();
        for s in spans {
            if let Some(Color::Rgb(r, g, b)) = s.style.fg {
                if s.style.add_modifier.contains(Modifier::BOLD) {
                    bold_rgb += 1;
                    colors.insert((r, g, b));
                }
            }
        }
        (bold_rgb, colors)
    }

    #[test]
    fn ultracode_gradient_highlights_keyword_visual_only() {
        let base = Style::default().fg(Color::White);
        let text = "please ultracode this";
        let spans = styled_spans_with_keyword_gradient(text, base);

        // Visual only: reassembled text is byte-identical to the input.
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, text);
        // Total display width is unchanged.
        assert_eq!(
            UnicodeWidthStr::width(joined.as_str()),
            UnicodeWidthStr::width(text)
        );

        // Each keyword character gets its own bold Rgb span, and the per-char
        // colors actually vary across the gradient.
        let (bold_rgb, colors) = bold_rgb_stats(&spans);
        assert_eq!(bold_rgb, "ultracode".len(), "one styled span per keyword char");
        assert!(colors.len() > 1, "gradient should vary per character");
    }

    #[test]
    fn ultracode_gradient_colors_are_unchanged() {
        // Regression guard: the ultracode red gradient must be byte-identical to
        // the original implementation (start #E91E63 → end #8C142D).
        let base = Style::default();
        let spans = styled_spans_with_keyword_gradient("ultracode", base);
        let first = spans.first().unwrap().style.fg;
        let last = spans.last().unwrap().style.fg;
        assert_eq!(first, Some(Color::Rgb(233, 30, 99)));
        assert_eq!(last, Some(Color::Rgb(140, 20, 45)));
    }

    #[test]
    fn gradient_untouched_without_keyword() {
        let base = Style::default().fg(Color::White);
        let text = "just a plain prompt here";
        let spans = styled_spans_with_keyword_gradient(text, base);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), text);
        assert_eq!(spans[0].style.fg, Some(Color::White));
        assert!(!spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn normal_keyword_gets_no_gradient() {
        // `normal` is a reset; it functions as an inline keyword for the turn
        // logic but must NOT be painted in the prompt box.
        let base = Style::default().fg(Color::White);
        let text = "back to normal now";
        let spans = styled_spans_with_keyword_gradient(text, base);
        assert_eq!(spans.len(), 1, "normal must not produce gradient spans");
        assert!(!spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn rocky_and_caveman_get_their_own_gradients() {
        let base = Style::default().fg(Color::White);

        // Rocky: grey/stone, dithered — one bold span per char, colors vary.
        let rocky = styled_spans_with_keyword_gradient("go rocky mode", base);
        let (bold, colors) = bold_rgb_stats(&rocky);
        assert_eq!(bold, "rocky".len());
        assert!(colors.len() > 1, "rocky gradient should vary per character");
        // Greyish: for at least one painted char, the channels are close together.
        assert!(
            colors.iter().any(|(r, g, b)| {
                let max = *r.max(g).max(b) as i16;
                let min = *r.min(g).min(b) as i16;
                (max - min) < 40
            }),
            "rocky should look grey/stony"
        );

        // Caveman: brown → green.
        let caveman = styled_spans_with_keyword_gradient("caveman please", base);
        let (bold, colors) = bold_rgb_stats(&caveman);
        assert_eq!(bold, "caveman".len());
        assert!(colors.len() > 1, "caveman gradient should vary per character");
        // First painted char leans brown (red channel dominant); the run ends
        // leaning green (green channel dominant).
        let first_painted = caveman
            .iter()
            .find_map(|s| match s.style.fg {
                Some(Color::Rgb(r, g, b)) if s.style.add_modifier.contains(Modifier::BOLD) => {
                    Some((r, g, b))
                }
                _ => None,
            })
            .unwrap();
        assert!(first_painted.0 > first_painted.1, "caveman starts brown");
    }

    #[test]
    fn gradient_handles_multiple_keywords_and_preserves_text() {
        let base = Style::default().fg(Color::White);
        let text = "ultracode then rocky again";
        let spans = styled_spans_with_keyword_gradient(text, base);
        let joined: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, text, "text preserved across multiple matches");
        let (bold, _) = bold_rgb_stats(&spans);
        // "ultracode" (9) + "rocky" (5) painted chars.
        assert_eq!(bold, "ultracode".len() + "rocky".len());
    }
}
