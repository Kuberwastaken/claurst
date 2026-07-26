// effort_picker.rs — horizontal, model-adaptive Effort selector for `/effort`.
//
// A horizontal "Faster → Smarter" track (issue #268). The selectable levels are
// model-adaptive: they come from `claurst_api::supported_efforts(provider,
// model, registry)`, which returns the model's supported ladder (ascending) with
// `Ultracode` always last. `Ultracode` is separated from the native levels by a
// `│` divider and rendered specially.
//
// The selector is DOCKED to the bottom of the screen as a full-width panel that
// takes the place of the prompt input while it is open (see `render_app` in
// `render.rs`): the prompt box is not drawn, and returns on confirm/cancel. It is
// no longer a small floating/centered modal.
//
// Layout (inside a bottom-docked, bordered "Effort" panel spanning the width):
//
//     Faster                                             Smarter
//     ────────────────────────────────────────────────────────
//                 low   medium   high   xhigh   max   │   ultracode
//                                     ▲
//     <description of the selected level>
//     ←/→ to adjust · Enter to confirm · Esc to cancel
//
// Selector-only visuals (never the prompt box): the selected label is bold and
// highlighted; the top native tier (`max`, or `xhigh` on models that don't
// expose `max`) is a per-character SOFT, DIFFUSED rainbow that gently animates
// with `frame_count` — so when a model exposes both, only `max` shimmers and
// `xhigh` stays a plain highlight; and `ultracode`, when selected, paints a bold claurst-red
// spectrum-analyzer audio wave as a background-color gradient (glowing bar tips,
// so text still sits cleanly on top, no cut-out boxes) framed by a gently
// breathing red outline.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders, Clear};
use ratatui::Frame;

use crate::model_picker::EffortLevel;

// ---------------------------------------------------------------------------
// Palette (selector-only) — claurst red family
// ---------------------------------------------------------------------------

/// Brighter red for the selected `ultracode` label / marker.
const RED_BRIGHT: Color = Color::Rgb(255, 105, 140);
/// Bright red used for the panel outline (a touch louder than `RED`).
const RED_BORDER: Color = Color::Rgb(255, 60, 120);
/// Dimmer red for the unselected `ultracode` label and the "Smarter" end.
const RED_DIM: Color = Color::Rgb(180, 78, 96);
/// Highlight for the selected (non-special) label.
const SELECTED_FG: Color = Color::Rgb(238, 238, 240);
/// Gray for unselected labels (off the spectrum).
const DIM_FG: Color = Color::Rgb(120, 120, 130);
/// Lighter warm-gray for unselected labels drawn ON the red wave (readable).
const LABEL_ON_WAVE: Color = Color::Rgb(212, 184, 194);
/// Near-white for the description / controls text drawn ON the red wave.
const DESC_ON_WAVE: Color = Color::Rgb(255, 236, 242);
/// The horizontal track line + divider.
const TRACK_FG: Color = Color::Rgb(90, 90, 104);
/// The "Faster" end label.
const FASTER_FG: Color = Color::Rgb(120, 160, 200);

/// Rows the docked panel wants (7 content rows + top/bottom border). Clamped to
/// the available height by the layout in `render_app`.
pub const DOCK_HEIGHT: u16 = 9;

/// Controls hint line.
const CONTROLS: &str = "\u{2190}/\u{2192} to adjust \u{b7} Enter to confirm \u{b7} Esc to cancel";
/// Spaces between adjacent labels / around the divider.
const SEP: usize = 3;

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Interactive state for the horizontal `/effort` selector.
#[derive(Debug, Default, Clone)]
pub struct EffortPickerState {
    pub visible: bool,
    /// The model-adaptive ordered ladder (ascending, `Ultracode` last).
    pub levels: Vec<EffortLevel>,
    /// Index into `levels` of the currently-highlighted level.
    pub selected: usize,
}

impl EffortPickerState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open the picker for the `current` effort, using `levels` as the
    /// model-adaptive ladder (as returned by `claurst_api::supported_efforts`).
    ///
    /// If `levels` is empty a sane default ladder is used. The selection is
    /// placed on `current` if present, otherwise on the nearest level at or below
    /// it (so switching from a model that supported `Max` to one that does not
    /// lands on the highest still-available level).
    pub fn open(&mut self, current: EffortLevel, levels: Vec<EffortLevel>) {
        let levels = if levels.is_empty() {
            default_levels()
        } else {
            levels
        };
        self.selected = index_for(&levels, current);
        self.levels = levels;
        self.visible = true;
    }

    pub fn close(&mut self) {
        self.visible = false;
    }

    /// Move the selection one step toward "Faster" (clamped at the low end).
    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move the selection one step toward "Smarter" (clamped at ultracode).
    pub fn select_next(&mut self) {
        if !self.levels.is_empty() {
            self.selected = (self.selected + 1).min(self.levels.len() - 1);
        }
    }

    /// The currently-selected level (falls back to `Medium` if empty).
    pub fn current(&self) -> EffortLevel {
        self.levels
            .get(self.selected)
            .copied()
            .unwrap_or(EffortLevel::Medium)
    }

    /// Whether the picker is showing an animated visual and so needs continuous
    /// repaints to keep moving. True for the `ultracode` spectrum background and
    /// any rainbow label. The CLI event loop uses this to keep ticking while the
    /// picker is open on an animated level. `xhigh` only animates when it is the
    /// top native tier (see [`is_rainbow_level`]).
    pub fn wants_animation(&self) -> bool {
        if !self.visible {
            return false;
        }
        let cur = self.current();
        cur.is_ultracode() || is_rainbow_level(cur, &self.levels)
    }
}

fn default_levels() -> Vec<EffortLevel> {
    vec![
        EffortLevel::Low,
        EffortLevel::Medium,
        EffortLevel::High,
        EffortLevel::Ultracode,
    ]
}

/// Choose the selected index for `current` within `levels`: an exact match if
/// present, otherwise the nearest level at or below it by rank, else the first.
fn index_for(levels: &[EffortLevel], current: EffortLevel) -> usize {
    if let Some(i) = levels.iter().position(|l| *l == current) {
        return i;
    }
    let want = rank(current);
    let mut best = 0usize;
    let mut best_rank = 0u8;
    for (i, l) in levels.iter().enumerate() {
        let r = rank(*l);
        if r <= want && r >= best_rank {
            best = i;
            best_rank = r;
        }
    }
    best
}

/// Ascending ordering rank used for nearest-level selection.
fn rank(level: EffortLevel) -> u8 {
    match level {
        EffortLevel::None => 0,
        EffortLevel::Minimal => 1,
        EffortLevel::Low => 2,
        EffortLevel::Medium => 3,
        EffortLevel::High => 4,
        EffortLevel::XHigh => 5,
        EffortLevel::Max => 6,
        EffortLevel::Ultracode => 7,
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render the horizontal `/effort` selector as a bottom-docked, full-width panel
/// laid out INSIDE the given `area` (typically the prompt input area). No
/// centering — the panel fills `area`. `frame_count` drives the animated
/// ultracode spectrum background and the animated `max` rainbow.
pub fn render_effort_picker(
    frame: &mut Frame,
    state: &EffortPickerState,
    area: Rect,
    frame_count: u64,
) {
    if !state.visible || state.levels.is_empty() || area.width < 4 || area.height < 3 {
        return;
    }
    let selected = state.selected.min(state.levels.len() - 1);
    let sel_level = state.levels[selected];
    // Ultracode paints the red audio wave behind everything; text drawn over it
    // needs brighter colors to stay readable.
    let on_spectrum = sel_level.is_ultracode();

    // Lay out the label row: styled spans, per-level center columns, total width.
    let (label_spans, centers, content_w) =
        layout_labels(&state.levels, selected, frame_count, on_spectrum);

    // A bottom-docked, full-width panel that occupies exactly `area`.
    frame.render_widget(Clear, area);
    // A brighter red outline; on the ultracode wave it breathes gently between
    // bright red and pink — a calm glow, never a hard flash.
    let border_color = if on_spectrum {
        let p = 0.5 + 0.5 * (frame_count as f32 * 0.05).sin();
        let g = (60.0 + 55.0 * p) as u8;
        let b = (120.0 + 45.0 * p) as u8;
        Color::Rgb(255, g, b)
    } else {
        RED_BORDER
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(
            " Effort ",
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let buf = frame.buffer_mut();

    // When ultracode is the selected level, paint the animated red waveform FIRST
    // (a dim field + a glowing crest line) so the labels/track/text drawn after it
    // sit clearly on top.
    if sel_level.is_ultracode() {
        paint_spectrum(buf, inner, frame_count);
    }

    // Content is laid out from a 1-cell pad inside the border.
    let x0 = inner.x + 1;
    let usable = inner.width.saturating_sub(2); // 1-cell pad on each side
    let cw = (content_w as u16).min(usable);
    // Center the label track within the full width for a balanced docked bar.
    let track_x = x0 + usable.saturating_sub(cw) / 2;

    // Row map (relative to inner.y):
    //   0 Faster..Smarter | 1 track | 2 labels | 3 marker
    //   4.. description (up to 2) | last inner row: controls
    let row = |i: u16| inner.y + i;
    let controls_row = inner.height.saturating_sub(1);

    // Faster / Smarter ends, spanning the full usable width.
    blit_str(buf, x0, row(0), "Faster", Style::default().fg(FASTER_FG), inner);
    let smarter = "Smarter";
    let sm_x = x0 + usable.saturating_sub(smarter.chars().count() as u16);
    let smarter_fg = if on_spectrum { LABEL_ON_WAVE } else { RED_DIM };
    blit_str(buf, sm_x, row(0), smarter, Style::default().fg(smarter_fg), inner);

    // Full-width track line.
    for dx in 0..usable {
        set_cell(buf, x0 + dx, row(1), '\u{2500}', Style::default().fg(TRACK_FG), inner);
    }

    // Level labels (centered track).
    for (col, span) in &label_spans {
        blit_span(buf, track_x + *col as u16, row(2), span, inner);
    }

    // Triangle marker directly under the selected level.
    let marker_x = track_x + centers[selected] as u16;
    set_cell(
        buf,
        marker_x,
        row(3),
        '\u{25b2}',
        Style::default()
            .fg(accent_for(sel_level, &state.levels, frame_count))
            .add_modifier(Modifier::BOLD),
        inner,
    );

    // Description of the selected level (word-wrapped) in the rows between the
    // marker and the bottom-anchored controls hint.
    // Over the red wave, description/controls use a near-white so they read
    // cleanly on the animated background (no dark cut-out boxes).
    let text_fg = if on_spectrum { DESC_ON_WAVE } else { DIM_FG };
    let desc_rows = controls_row.saturating_sub(4).min(2) as usize;
    if desc_rows > 0 {
        let desc = level_description(sel_level, &state.levels);
        for (i, line) in word_wrap(&desc, usable as usize)
            .into_iter()
            .take(desc_rows)
            .enumerate()
        {
            blit_str(
                buf,
                x0,
                row(4 + i as u16),
                &line,
                Style::default().fg(text_fg),
                inner,
            );
        }
    }

    // Controls hint, anchored to the bottom inner row.
    blit_str(buf, x0, row(controls_row), CONTROLS, Style::default().fg(text_fg), inner);
}

/// Whether a level should get the per-character shimmering rainbow treatment.
/// `max` always does; `xhigh` only when it is the top native tier — i.e. the
/// model's ladder has no `max` above it. So on models that expose both
/// `xhigh` and `max` (e.g. Claude), `xhigh` stays a plain highlight and the
/// rainbow is reserved for `max`.
fn is_rainbow_level(level: EffortLevel, levels: &[EffortLevel]) -> bool {
    match level {
        EffortLevel::Max => true,
        EffortLevel::XHigh => !levels.contains(&EffortLevel::Max),
        _ => false,
    }
}

/// The accent color for a level's marker (matches its label styling). Rainbow
/// tiers cycle through the animated rainbow so the marker shimmers with them.
fn accent_for(level: EffortLevel, levels: &[EffortLevel], frame_count: u64) -> Color {
    if is_rainbow_level(level, levels) {
        let (r, g, b) = hsv_to_rgb((frame_count as f32 * 6.0).rem_euclid(360.0), 0.9, 1.0);
        return Color::Rgb(r, g, b);
    }
    if level.is_ultracode() {
        return RED_BRIGHT;
    }
    SELECTED_FG
}

/// Build the label row: placed styled spans (`(col_offset, span)`), the center
/// column of each level (for marker alignment), and the total content width.
/// `frame_count` animates the `max` rainbow.
fn layout_labels(
    levels: &[EffortLevel],
    selected: usize,
    frame_count: u64,
    on_spectrum: bool,
) -> (Vec<(usize, Span<'static>)>, Vec<usize>, usize) {
    let mut placed: Vec<(usize, Span<'static>)> = Vec::new();
    let mut centers = vec![0usize; levels.len()];
    let mut col = 0usize;
    let mut first = true;
    for (i, lvl) in levels.iter().enumerate() {
        // Ultracode is fenced off from the native ladder by a divider.
        if lvl.is_ultracode() {
            if !first {
                col += SEP;
            }
            placed.push((col, Span::styled("\u{2502}".to_string(), Style::default().fg(TRACK_FG))));
            col += 1;
            first = false;
        }
        if !first {
            col += SEP;
        }
        first = false;

        let start = col;
        let width = lvl.label().chars().count();
        centers[i] = start + width / 2;
        for span in styled_label(*lvl, levels, i == selected, frame_count, on_spectrum) {
            let w = span.content.chars().count();
            placed.push((col, span));
            col += w;
        }
    }
    (placed, centers, col)
}

/// Style a single level label. Non-selected labels are dim (lighter over the
/// wave); the selected one is highlighted, with `ultracode` red and the rainbow
/// tiers (see [`is_rainbow_level`]) a per-char shimmer that animates with
/// `frame_count`.
fn styled_label(
    level: EffortLevel,
    levels: &[EffortLevel],
    selected: bool,
    frame_count: u64,
    on_spectrum: bool,
) -> Vec<Span<'static>> {
    let text = level.label();
    if level.is_ultracode() {
        let fg = if selected { RED_BRIGHT } else { RED_DIM };
        let mut st = Style::default().fg(fg);
        if selected {
            st = st.add_modifier(Modifier::BOLD);
        }
        return vec![Span::styled(text.to_string(), st)];
    }
    if !selected {
        // On the wave, bold + a light warm-gray keeps unselected labels clearly
        // legible ON TOP of the animated background.
        let mut st = Style::default().fg(if on_spectrum { LABEL_ON_WAVE } else { DIM_FG });
        if on_spectrum {
            st = st.add_modifier(Modifier::BOLD);
        }
        return vec![Span::styled(text.to_string(), st)];
    }
    if is_rainbow_level(level, levels) {
        // Top-tier native levels (just below ultracode) shimmer rainbow.
        return rainbow_spans(text, frame_count);
    }
    vec![Span::styled(
        text.to_string(),
        Style::default().fg(SELECTED_FG).add_modifier(Modifier::BOLD),
    )]
}

/// One bold span per character forming a SOFT, DIFFUSED rainbow across the word:
/// pastel saturation and only a narrow slice of the spectrum spread over the
/// word, so neighbouring characters differ just slightly (they blend rather than
/// jump — the blurry, Claude-Code-ish look). It drifts and breathes gently with
/// `frame_count`. Used for the `xhigh`/`max` labels — the tiers just below
/// ultracode.
fn rainbow_spans(text: &str, frame_count: u64) -> Vec<Span<'static>> {
    let n = text.chars().count().max(1);
    // Slow hue drift over time.
    let phase = frame_count as f32 * 3.0;
    let t = frame_count as f32;
    text.chars()
        .enumerate()
        .map(|(i, ch)| {
            // Only ~150° of hue spread across the whole word → gentle transitions.
            let hue = 150.0 * i as f32 / n as f32 + phase;
            // Soft pastel: moderate saturation + a gentle brightness breathe.
            let v = (0.86 + 0.10 * (t * 0.18 + i as f32 * 0.7).sin()).clamp(0.0, 1.0);
            let (r, g, b) = hsv_to_rgb(hue, 0.55, v);
            Span::styled(
                ch.to_string(),
                Style::default()
                    .fg(Color::Rgb(r, g, b))
                    .add_modifier(Modifier::BOLD),
            )
        })
        .collect()
}

/// Convert HSV (`h` in degrees, `s`/`v` in `[0, 1]`) to an 8-bit RGB triple.
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let hp = (h.rem_euclid(360.0)) / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp as u8 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    (
        ((r1 + m) * 255.0).round() as u8,
        ((g1 + m) * 255.0).round() as u8,
        ((b1 + m) * 255.0).round() as u8,
    )
}

/// The description shown for the selected level. Ultracode's description is
/// derived from the model's top native effort: "<top> + workflows".
fn level_description(level: EffortLevel, levels: &[EffortLevel]) -> String {
    match level {
        EffortLevel::None => {
            "No reasoning \u{2014} the model answers directly with thinking disabled.".to_string()
        }
        EffortLevel::Minimal => {
            "The smallest reasoning budget \u{2014} a touch of thinking for the quickest tasks."
                .to_string()
        }
        EffortLevel::Low => {
            "Fastest, most direct responses. Best for simple edits and quick questions.".to_string()
        }
        EffortLevel::Medium => {
            "Balanced reasoning and speed \u{2014} a solid default for everyday work.".to_string()
        }
        EffortLevel::High => {
            "Deeper, more careful reasoning for trickier, multi-step problems.".to_string()
        }
        EffortLevel::XHigh => {
            "Extended thinking budget for hard problems that need more deliberation.".to_string()
        }
        EffortLevel::Max => "May use excessive tokens resulting in long response times or \
             overthinking. Use sparingly for the hardest tasks."
            .to_string(),
        EffortLevel::Ultracode => {
            let top = top_native_label(levels);
            format!("{top} + workflows: bounded delegation across native primitives with verification.")
        }
    }
}

/// The label of the highest non-ultracode level in `levels` (the model's top
/// native effort), used to describe ultracode as "<top> + workflows".
fn top_native_label(levels: &[EffortLevel]) -> &'static str {
    levels
        .iter()
        .rev()
        .find(|l| !l.is_ultracode())
        .map(|l| l.label())
        .unwrap_or("max")
}

// ---------------------------------------------------------------------------
// Buffer helpers
// ---------------------------------------------------------------------------

/// Set a single cell's glyph + style, clipped to `inner`.
fn set_cell(buf: &mut Buffer, x: u16, y: u16, ch: char, style: Style, inner: Rect) {
    if !(inner.left()..inner.right()).contains(&x) || !(inner.top()..inner.bottom()).contains(&y) {
        return;
    }
    if let Some(cell) = buf.cell_mut((x, y)) {
        cell.set_char(ch);
        cell.set_style(style);
    }
}

/// Write a string starting at `(x, y)`, one cell per char, clipped to `inner`.
fn blit_str(buf: &mut Buffer, x: u16, y: u16, s: &str, style: Style, inner: Rect) {
    let mut cx = x;
    for ch in s.chars() {
        set_cell(buf, cx, y, ch, style, inner);
        cx = cx.saturating_add(1);
    }
}

/// Write a styled span starting at `(x, y)`.
fn blit_span(buf: &mut Buffer, x: u16, y: u16, span: &Span, inner: Rect) {
    blit_str(buf, x, y, span.content.as_ref(), span.style, inner);
}

/// Minimal greedy word-wrap to `width` columns.
fn word_wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        if cur.is_empty() {
            cur.push_str(word);
        } else if cur.chars().count() + 1 + word.chars().count() <= width {
            cur.push(' ');
            cur.push_str(word);
        } else {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(word);
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

// ---------------------------------------------------------------------------
// Ultracode red wave (background)
// ---------------------------------------------------------------------------

/// Paint claurst's red audio wave into `inner` as a BACKGROUND-color gradient
/// (space glyphs). It reads like a glowing oscilloscope waveform: a bright,
/// undulating crest LINE that flows slowly sideways over a dim deep-red field,
/// with a dark wash above. Because the field stays dim and only the thin crest
/// glows, foreground text clearly sits ON TOP (no washout) with no cut-out boxes.
/// `frame_count` is the animation phase.
fn paint_spectrum(buf: &mut Buffer, inner: Rect, frame_count: u64) {
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let height = inner.height as f32;
    for gx in 0..inner.width {
        let amp = spectrum_amp(gx, frame_count).clamp(0.0, 1.0);
        let crest = amp * height; // the waveform line, in rows from the bottom
        let x = inner.left() + gx;
        for r in 0..inner.height {
            let rf = r as f32; // rows up from the bottom (0 = bottom)
            // The field: a dim red below the crest (a touch brighter near it) and
            // a darker wash above — always low so text stays dominant.
            let field = if rf <= crest {
                let depth = (crest - rf) / crest.max(1.0); // 0 at crest -> 1 at base
                0.30 - 0.12 * depth
            } else {
                (0.14 - 0.04 * (rf - crest)).clamp(0.05, 0.14)
            };
            // The glowing crest line: a soft bright band within ~2 rows of the crest.
            let glow = (1.0 - (rf - crest).abs() / 2.0).clamp(0.0, 1.0);
            let lit = (field + 0.7 * glow).clamp(0.0, 1.0);
            let shade = red_shade(lit);
            let y = inner.bottom() - 1 - r;
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_char(' ');
                cell.set_fg(shade);
                cell.set_bg(shade);
            }
        }
    }
}

/// Per-column crest height in `[0.25, 0.85]` for a given column and frame. A
/// smooth travelling wave plus a broad second harmonic make an undulating
/// waveform that flows slowly sideways; the range keeps the crest inside the
/// panel with headroom above and a field below. `frame` moves the phase slowly —
/// gentle, wavy motion, not a fast flicker.
fn spectrum_amp(gx: u16, frame: u64) -> f32 {
    let fx = gx as f32;
    let t = frame as f32;
    let a = 0.60 * (fx * 0.28 - t * 0.055).sin()
        + 0.40 * (fx * 0.13 + t * 0.035 + 1.1).sin();
    // Map the [-1, 1] wave into [0.25, 0.85] of the panel height.
    0.25 + 0.60 * (0.5 + 0.5 * a)
}

/// A claurst-red whose brightness scales with `lit` in `[0, 1]`: a deep-red wash
/// at the base brightening to a vivid claurst red at the crest. Used as a
/// BACKGROUND color for the wave (so it can be richly red while text stays
/// readable on top). Always red-dominant (`r > g` and `r > b`) — never purple.
fn red_shade(lit: f32) -> Color {
    let lit = lit.clamp(0.0, 1.0);
    // Deep-red wash (34, 8, 16) -> vivid claurst red (255, 42, 104).
    let r = 34.0 + 221.0 * lit;
    let g = 8.0 + 34.0 * lit;
    let b = 16.0 + 88.0 * lit;
    Color::Rgb(r as u8, g as u8, b as u8)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn full_ladder() -> Vec<EffortLevel> {
        vec![
            EffortLevel::Low,
            EffortLevel::Medium,
            EffortLevel::High,
            EffortLevel::XHigh,
            EffortLevel::Max,
            EffortLevel::Ultracode,
        ]
    }

    fn state_with(levels: Vec<EffortLevel>, selected: usize) -> EffortPickerState {
        EffortPickerState {
            visible: true,
            levels,
            selected,
        }
    }

    fn render_to_buffer(state: &EffortPickerState, frame_count: u64) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|f| render_effort_picker(f, state, f.area(), frame_count))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    /// Each buffer row as a `String` of cell glyphs (all glyphs here are 1 cell
    /// wide, so a char index equals its column).
    fn buffer_rows(buf: &Buffer) -> Vec<String> {
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .filter_map(|x| buf.cell((x, y)).map(|c| c.symbol().to_string()))
                    .collect::<String>()
            })
            .collect()
    }

    /// Char-column index of `needle` in `row` (converts the byte offset from
    /// `str::find` to a char/column index, since labels can share a row with
    /// multi-byte glyphs like the border/divider `│`).
    fn char_col_of(row: &str, needle: &str) -> Option<usize> {
        let byte_idx = row.find(needle)?;
        Some(row[..byte_idx].chars().count())
    }

    #[test]
    fn open_selects_current_and_clamps_navigation() {
        let mut s = EffortPickerState::new();
        s.open(EffortLevel::High, full_ladder());
        assert!(s.visible);
        assert_eq!(s.current(), EffortLevel::High);

        // ← past the start clamps at Low.
        for _ in 0..10 {
            s.select_prev();
        }
        assert_eq!(s.current(), EffortLevel::Low);
        // → past the end clamps at Ultracode.
        for _ in 0..20 {
            s.select_next();
        }
        assert_eq!(s.current(), EffortLevel::Ultracode);
        assert!(s.wants_animation());
    }

    #[test]
    fn open_falls_back_to_nearest_available_level() {
        // Model without Max/XHigh; opening on Max lands on the highest native.
        let levels = vec![
            EffortLevel::Low,
            EffortLevel::Medium,
            EffortLevel::High,
            EffortLevel::Ultracode,
        ];
        let mut s = EffortPickerState::new();
        s.open(EffortLevel::Max, levels);
        assert_eq!(s.current(), EffortLevel::High);
    }

    /// A native ladder that tops out at `xhigh` (no `max`), plus `ultracode`.
    fn xhigh_top_ladder() -> Vec<EffortLevel> {
        vec![
            EffortLevel::Low,
            EffortLevel::Medium,
            EffortLevel::High,
            EffortLevel::XHigh,
            EffortLevel::Ultracode,
        ]
    }

    #[test]
    fn wants_animation_only_for_top_rainbow_tier_and_ultracode() {
        // `max` and `ultracode` always animate; `xhigh` animates only when it is
        // the top native tier (no `max` above it).
        let mut s = EffortPickerState::new();
        s.open(EffortLevel::Medium, full_ladder());
        assert!(!s.wants_animation(), "medium is static");
        s.selected = 2; // high
        assert!(!s.wants_animation(), "high is static");
        s.selected = 3; // xhigh — max is present, so static
        assert!(!s.wants_animation(), "xhigh static when max is present");
        s.selected = 4; // max
        assert!(s.wants_animation(), "max rainbow shimmers");
        s.selected = 5; // ultracode
        assert!(s.wants_animation(), "ultracode wave animates");

        // On a ladder without `max`, `xhigh` is the top tier and shimmers.
        let mut s2 = EffortPickerState::new();
        s2.open(EffortLevel::XHigh, xhigh_top_ladder());
        assert_eq!(s2.current(), EffortLevel::XHigh);
        assert!(s2.wants_animation(), "xhigh shimmers when it is the top tier");
    }

    #[test]
    fn xhigh_rainbows_only_when_it_is_the_top_tier() {
        // Ladder WITHOUT max: xhigh is the top native tier → per-char rainbow
        // that shifts across frames.
        let state = state_with(xhigh_top_ladder(), 3); // xhigh selected, no max
        let a = render_to_buffer(&state, 0);
        let b = render_to_buffer(&state, 20);
        let rows = buffer_rows(&a);
        let label_y = rows
            .iter()
            .position(|r| r.contains("ultracode"))
            .expect("label row present");
        let start = char_col_of(&rows[label_y], "xhigh").expect("xhigh in labels row");
        let y = label_y as u16;
        let c0 = a.cell((start as u16, y)).unwrap().fg;
        let c1 = a.cell((start as u16 + 1, y)).unwrap().fg;
        assert_ne!(c0, c1, "top-tier xhigh rainbow chars must differ: {c0:?} vs {c1:?}");
        let cb = b.cell((start as u16, y)).unwrap().fg;
        assert_ne!(c0, cb, "top-tier xhigh rainbow should animate: {c0:?} vs {cb:?}");

        // Ladder WITH max: xhigh is no longer the top → plain solid highlight
        // (all chars share one color; the rainbow is reserved for max).
        let state2 = state_with(full_ladder(), 3); // xhigh selected, max present
        let f = render_to_buffer(&state2, 0);
        let rows2 = buffer_rows(&f);
        let ly2 = rows2
            .iter()
            .position(|r| r.contains("ultracode"))
            .expect("label row present");
        let sx = char_col_of(&rows2[ly2], "xhigh").expect("xhigh in labels row");
        let y2 = ly2 as u16;
        let d0 = f.cell((sx as u16, y2)).unwrap().fg;
        let d1 = f.cell((sx as u16 + 1, y2)).unwrap().fg;
        assert_eq!(d0, d1, "xhigh with max present should be a solid color: {d0:?} vs {d1:?}");
    }

    #[test]
    fn ultracode_wave_is_a_background_gradient_under_text() {
        // The description text sits ON the red wave: its cells carry a red-ish
        // background (not a dark cut-out box) while still showing the glyphs.
        let state = state_with(full_ladder(), 5); // ultracode
        let buf = render_to_buffer(&state, 3);
        let rows = buffer_rows(&buf);
        // Find a row containing the description sentence.
        let (dy, drow) = rows
            .iter()
            .enumerate()
            .find(|(_, r)| r.contains("workflows"))
            .map(|(i, r)| (i as u16, r.clone()))
            .expect("description row present");
        let cx = char_col_of(&drow, "workflows").unwrap() as u16;
        match buf.cell((cx, dy)).unwrap().bg {
            Color::Rgb(r, _g, b) => {
                assert!(r > b, "text must sit on the red wave bg, got r={r} b={b}")
            }
            other => panic!("expected an Rgb wave background under text, got {other:?}"),
        }
    }

    #[test]
    fn renders_docked_in_given_bottom_rect() {
        // Render into a bottom-docked rect within a taller buffer; the panel must
        // fill exactly that rect (full width at the docked y), NOT be centered.
        let state = state_with(full_ladder(), 5); // ultracode
        let mut terminal = Terminal::new(TestBackend::new(60, 14)).unwrap();
        let area = Rect { x: 0, y: 5, width: 60, height: DOCK_HEIGHT };
        terminal
            .draw(|f| render_effort_picker(f, &state, area, 0))
            .unwrap();
        let buf = terminal.backend().buffer().clone();

        // Border corners sit exactly on the rect edges — a bottom-docked panel,
        // not a small centered modal.
        assert_eq!(buf.cell((0, 5)).unwrap().symbol(), "\u{250c}", "top-left at rect origin");
        assert_eq!(buf.cell((59, 5)).unwrap().symbol(), "\u{2510}", "top-right at rect edge");
        assert_eq!(buf.cell((0, 13)).unwrap().symbol(), "\u{2514}", "bottom-left at rect bottom");
        assert_eq!(buf.cell((59, 13)).unwrap().symbol(), "\u{2518}", "bottom-right at rect corner");

        // Nothing is drawn above the docked rect (a centered modal would).
        let top_row: String = (0..60)
            .map(|x| buf.cell((x, 0)).unwrap().symbol().to_string())
            .collect();
        assert!(top_row.trim().is_empty(), "no content above the dock: {top_row:?}");

        // The selector content is present within the panel.
        let rows = buffer_rows(&buf);
        assert!(
            rows.iter().any(|r| r.contains("ultracode")),
            "labels present inside the docked panel"
        );
    }

    #[test]
    fn renders_model_levels_and_ultracode_after_divider() {
        // Max selected → no spectrum, so label gaps read as plain spaces.
        let state = state_with(full_ladder(), 4);
        let rows = buffer_rows(&render_to_buffer(&state, 0));
        let label_row = rows
            .iter()
            .find(|r| r.contains("ultracode"))
            .expect("label row present");

        for lbl in ["low", "medium", "high", "xhigh", "max"] {
            assert!(label_row.contains(lbl), "labels row missing {lbl}: {label_row:?}");
        }
        // A divider must sit between `max` and `ultracode`.
        let max_end = label_row.find("max").unwrap() + "max".len();
        let uc = label_row.find("ultracode").unwrap();
        let gap = &label_row[max_end..uc];
        assert!(
            gap.contains('\u{2502}'),
            "expected `│` divider between max and ultracode, gap={gap:?}"
        );
    }

    #[test]
    fn marker_sits_under_selected_level() {
        // Select `medium` (unique, not a substring of another label).
        let state = state_with(full_ladder(), 1);
        let rows = buffer_rows(&render_to_buffer(&state, 0));

        let (marker_y, marker_row) = rows
            .iter()
            .enumerate()
            .find(|(_, r)| r.contains('\u{25b2}'))
            .map(|(i, r)| (i, r.clone()))
            .expect("marker row present");
        let marker_col = marker_row.chars().position(|c| c == '\u{25b2}').unwrap();

        let label_row = &rows[marker_y - 1];
        let start = char_col_of(label_row, "medium").expect("medium in labels row");
        let end = start + "medium".chars().count();
        assert!(
            marker_col >= start && marker_col < end,
            "marker col {marker_col} not within medium [{start}, {end})"
        );
    }

    #[test]
    fn max_uses_distinct_per_char_rainbow_colors() {
        let state = state_with(full_ladder(), 4); // max selected
        let buf = render_to_buffer(&state, 0);
        let rows = buffer_rows(&buf);
        let label_y = rows
            .iter()
            .position(|r| r.contains("ultracode"))
            .expect("label row present");
        let label_row = &rows[label_y];
        let start = char_col_of(label_row, "max").expect("max in labels row");

        let y = label_y as u16;
        let colors: Vec<Color> = (0..3u16)
            .map(|dx| buf.cell((start as u16 + dx, y)).expect("max cell").fg)
            .collect();
        assert_ne!(colors[0], colors[1], "rainbow chars must differ: {colors:?}");
        assert_ne!(colors[1], colors[2], "rainbow chars must differ: {colors:?}");
        assert_ne!(colors[0], colors[2], "rainbow chars must differ: {colors:?}");
    }

    #[test]
    fn max_rainbow_animates_across_frames() {
        // The `max` label's per-char colors shift with frame_count.
        let state = state_with(full_ladder(), 4); // max selected
        let a = render_to_buffer(&state, 0);
        let b = render_to_buffer(&state, 20);
        let rows_a = buffer_rows(&a);
        let label_y = rows_a
            .iter()
            .position(|r| r.contains("ultracode"))
            .expect("label row present");
        let start = char_col_of(&rows_a[label_y], "max").expect("max col");
        let y = label_y as u16;
        let ca = a.cell((start as u16, y)).unwrap().fg;
        let cb = b.cell((start as u16, y)).unwrap().fg;
        assert_ne!(ca, cb, "max rainbow should animate between frames {ca:?} vs {cb:?}");
    }

    #[test]
    fn wave_shades_are_red_not_purple() {
        // The ultracode wave stays red-dominant across the whole brightness range
        // (a purple would have blue >= red) and gets richly red at the crest.
        for lit in [0.0f32, 0.35, 0.6, 1.0] {
            match red_shade(lit) {
                Color::Rgb(r, g, b) => {
                    assert!(r > g && r > b, "shade must be red-dominant: {r},{g},{b} @ {lit}")
                }
                other => panic!("expected Rgb, got {other:?}"),
            }
        }
        // The ultracode label / accent colors are all red-family too.
        for c in [RED_BRIGHT, RED_DIM, RED_BORDER] {
            match c {
                Color::Rgb(r, g, b) => {
                    assert!(r > g && r > b, "label color must be red: {r},{g},{b}")
                }
                other => panic!("expected Rgb, got {other:?}"),
            }
        }
    }

    #[test]
    fn ultracode_spectrum_animates_but_others_are_static() {
        let levels = vec![
            EffortLevel::Low,
            EffortLevel::Medium,
            EffortLevel::High,
            EffortLevel::Ultracode,
        ];

        // Ultracode selected → background differs between two frame_count values.
        let ultra = state_with(levels.clone(), levels.len() - 1);
        let a = render_to_buffer(&ultra, 0);
        let b = render_to_buffer(&ultra, 30);
        assert_ne!(
            a.content(),
            b.content(),
            "ultracode spectrum should animate between frames"
        );

        // High selection → no spectrum, no rainbow, identical across frames.
        let high = state_with(levels, 2);
        let c = render_to_buffer(&high, 0);
        let d = render_to_buffer(&high, 30);
        assert_eq!(
            c.content(),
            d.content(),
            "non-animated picker must not change between frames"
        );
    }
}
