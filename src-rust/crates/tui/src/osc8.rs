//! OSC 8 hyperlink overlay for the ratatui-based TUI.
//!
//! ratatui 0.29 has no native hyperlink primitive — its render path writes
//! one Cell at a time and never carries OSC 8 state across cells. We hook
//! the main draw loop, scan the painted buffer for URLs, and re-emit just
//! those cells wrapped in `OSC 8 ; ; URL ESC \` ... `OSC 8 ;; ESC \` so
//! terminals that implement the protocol (Windows Terminal, iTerm2,
//! WezTerm, Kitty, Konsole, VS Code, …) make them Ctrl/Cmd-clickable.
//! Terminals without OSC 8 support silently ignore the unknown OSC.
//!
//! The detection mirrors `messages::markdown::URL_PATTERN`, so what the
//! markdown renderer underlines in cyan is exactly what gets linked here.
//!
//! Disable with `CLAURST_NO_HYPERLINKS=1`.

use std::io::{self, Write};

use crossterm::{
    cursor::{MoveTo, RestorePosition, SavePosition},
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    QueueableCommand,
};
use once_cell::sync::Lazy;
use ratatui::buffer::Buffer;
use regex::Regex;

const OSC8_OPEN_PREFIX: &str = "\x1b]8;;";
const OSC8_ST: &str = "\x1b\\";
const OSC8_CLOSE: &str = "\x1b]8;;\x1b\\";

static URL_RE: Lazy<Regex> = Lazy::new(|| {
    // Conservative URL set — restricting body chars to the RFC 3986 reserved/
    // unreserved alphabet avoids accidentally pulling in trailing whitespace
    // or display punctuation that the markdown styler also rejects.
    Regex::new(
        r#"(?:https?|ftp)://[A-Za-z0-9\-._~:/?#\[\]@!$&'()*+,;=%]+|www\.[A-Za-z0-9\-]+\.[A-Za-z0-9\-._~:/?#\[\]@!$&'()*+,;=%]+"#,
    )
    .expect("OSC 8 URL regex")
});

#[derive(Debug, Clone)]
pub struct UrlHit {
    /// Visual column of the first cell, absolute (already includes area.x).
    pub col: u16,
    /// Visual row, absolute (already includes area.y).
    pub row: u16,
    /// URL passed to the terminal — normalized (e.g., `www.…` → `https://…`).
    pub url: String,
    /// Original on-screen text — re-printed verbatim so the row looks unchanged.
    pub display: String,
}

fn enabled() -> bool {
    enabled_for(std::env::var("CLAURST_NO_HYPERLINKS").ok().as_deref())
}

/// Pure decision split out from [`enabled`] so it can be unit-tested without
/// mutating the process-global env var (which races with the URL-scan tests
/// running in parallel and made them flaky).
fn enabled_for(value: Option<&str>) -> bool {
    match value {
        Some(v) => !matches!(v.trim(), "1" | "true" | "yes" | "on"),
        None => true,
    }
}

/// Strip trailing punctuation that is almost certainly *not* part of the URL.
/// Parentheses are only stripped when unbalanced — `https://en.wikipedia.org/wiki/Foo_(bar)`
/// should keep its closing paren.
fn trim_url_punct(matched: &str) -> &str {
    let bytes = matched.as_bytes();
    let mut paren_balance: i32 = 0;
    for &b in bytes {
        if b == b'(' {
            paren_balance += 1;
        } else if b == b')' {
            paren_balance -= 1;
        }
    }
    let mut end = bytes.len();
    while end > 0 {
        let last = bytes[end - 1];
        let strip = match last {
            b'.' | b',' | b';' | b':' | b'!' | b'?' | b'\'' | b'"' | b'>' => true,
            b']' | b'}' => true,
            b')' => paren_balance < 0,
            _ => false,
        };
        if strip {
            if last == b')' {
                paren_balance += 1;
            }
            end -= 1;
        } else {
            break;
        }
    }
    &matched[..end]
}

/// Scan a buffer that's just been rendered (e.g. `CompletedFrame::buffer`
/// or `Frame::buffer_mut()` inside a `Terminal::draw` closure) for URL
/// runs and return their visual positions + normalized targets.
///
/// **Important**: do NOT call this on `Terminal::current_buffer_mut()`
/// after `draw()` — ratatui swaps buffers at the end of draw and
/// `current_buffer_mut()` then points at the cleared next-frame slot.
/// Use the `CompletedFrame` returned by `draw()` instead.
pub fn scan_buffer_for_urls(buf: &Buffer) -> Vec<UrlHit> {
    if !enabled() {
        return Vec::new();
    }
    scan_buffer(buf)
}

/// Write OSC 8 hyperlink wrappers for the given hits to stdout. Saves and
/// restores the cursor position so the user-visible cursor stays where
/// ratatui put it. No-op when `hits` is empty or hyperlinks are disabled.
pub fn emit_hits(hits: &[UrlHit]) -> io::Result<()> {
    if !enabled() || hits.is_empty() {
        return Ok(());
    }
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    write_hits(&mut lock, hits)
}

fn scan_buffer(buf: &Buffer) -> Vec<UrlHit> {
    let area = buf.area();
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }

    let mut hits = Vec::new();
    let mut row_text = String::new();
    // Maps each byte index of `row_text` back to its visual column. Wide
    // cells (CJK, emoji) leave a width-1 contribution per pushed byte; the
    // continuation cell at col+1 has an empty symbol and is skipped.
    let mut col_of_byte: Vec<u16> = Vec::new();

    for row in 0..area.height {
        row_text.clear();
        col_of_byte.clear();
        for col in 0..area.width {
            let cell = &buf[(area.x + col, area.y + row)];
            let sym = cell.symbol();
            if sym.is_empty() {
                continue;
            }
            let before = row_text.len();
            row_text.push_str(sym);
            for _ in before..row_text.len() {
                col_of_byte.push(col);
            }
        }

        for m in URL_RE.find_iter(&row_text) {
            let matched = &row_text[m.start()..m.end()];
            let cleaned = trim_url_punct(matched);
            if cleaned.is_empty() {
                continue;
            }
            let start_byte = m.start();
            // Defensive: a regex match should always be in bounds; if not, skip.
            let Some(&start_col) = col_of_byte.get(start_byte) else {
                continue;
            };
            hits.push(UrlHit {
                col: area.x + start_col,
                row: area.y + row,
                url: normalize_url(cleaned),
                display: cleaned.to_string(),
            });
        }
    }
    hits
}

fn normalize_url(s: &str) -> String {
    if s.starts_with("www.") {
        format!("https://{s}")
    } else {
        s.to_string()
    }
}

fn write_hits(writer: &mut impl Write, hits: &[UrlHit]) -> io::Result<()> {
    writer.queue(SavePosition)?;
    for h in hits {
        writer.queue(MoveTo(h.col, h.row))?;
        // Match the markdown renderer's URL styling so the overlay looks
        // identical to ratatui's original paint of these cells.
        writer.queue(SetForegroundColor(Color::Cyan))?;
        writer.queue(SetAttribute(Attribute::Underlined))?;
        writer.queue(Print(format!("{OSC8_OPEN_PREFIX}{}{OSC8_ST}", h.url)))?;
        writer.queue(Print(&h.display))?;
        writer.queue(Print(OSC8_CLOSE))?;
        writer.queue(SetAttribute(Attribute::NoUnderline))?;
        writer.queue(ResetColor)?;
    }
    writer.queue(RestorePosition)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Style;

    fn buffer_with(lines: &[&str]) -> Buffer {
        let h = lines.len() as u16;
        let w = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
        let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
        for (y, line) in lines.iter().enumerate() {
            buf.set_string(0, y as u16, *line, Style::default());
        }
        buf
    }

    #[test]
    fn detects_simple_http_url() {
        let buf = buffer_with(&["Visit https://example.com today"]);
        let hits = scan_buffer_for_urls(&buf);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].display, "https://example.com");
        assert_eq!(hits[0].url, "https://example.com");
        assert_eq!(hits[0].col, 6);
        assert_eq!(hits[0].row, 0);
    }

    #[test]
    fn strips_trailing_period_and_paren() {
        let buf = buffer_with(&["See (https://example.com)."]);
        let hits = scan_buffer_for_urls(&buf);
        assert_eq!(hits.len(), 1, "hits: {hits:?}");
        assert_eq!(hits[0].display, "https://example.com");
    }

    #[test]
    fn keeps_balanced_paren_inside_url() {
        let buf = buffer_with(&["see https://en.wikipedia.org/wiki/Foo_(bar) ok"]);
        let hits = scan_buffer_for_urls(&buf);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].display, "https://en.wikipedia.org/wiki/Foo_(bar)");
    }

    #[test]
    fn detects_www_and_normalizes_to_https() {
        let buf = buffer_with(&["go to www.example.com now"]);
        let hits = scan_buffer_for_urls(&buf);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].display, "www.example.com");
        assert_eq!(hits[0].url, "https://www.example.com");
    }

    #[test]
    fn no_urls_no_hits() {
        let buf = buffer_with(&["just some text without urls"]);
        let hits = scan_buffer_for_urls(&buf);
        assert!(hits.is_empty());
    }

    #[test]
    fn two_urls_one_line() {
        let buf = buffer_with(&["a https://one.test and https://two.test x"]);
        let hits = scan_buffer_for_urls(&buf);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].display, "https://one.test");
        assert_eq!(hits[1].display, "https://two.test");
        // Columns advance as expected.
        assert!(hits[1].col > hits[0].col);
    }

    #[test]
    fn handles_share_url_with_hash_fragment() {
        let url = "https://claurst.kuber.studio/session/#c2cc4dd0ae0d3fa6dc7ab21f2a79d7a1";
        let line = format!("Share URL: {url}");
        let buf = buffer_with(&[&line]);
        let hits = scan_buffer_for_urls(&buf);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].display, url);
    }

    #[test]
    fn write_hits_emits_osc8_envelope() {
        let hits = vec![UrlHit {
            col: 6,
            row: 0,
            url: "https://example.com".to_string(),
            display: "https://example.com".to_string(),
        }];
        let mut out: Vec<u8> = Vec::new();
        write_hits(&mut out, &hits).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("\x1b]8;;https://example.com\x1b\\"), "missing OSC 8 open in: {s:?}");
        assert!(s.contains("\x1b]8;;\x1b\\"), "missing OSC 8 close in: {s:?}");
        assert!(s.contains("https://example.com"));
    }

    #[test]
    fn write_hits_no_output_when_empty() {
        // Sanity: emit_hyperlinks bails before calling write_hits when there
        // are no hits, but the function itself should still be cheap.
        let mut out: Vec<u8> = Vec::new();
        write_hits(&mut out, &[]).unwrap();
        // Even with no hits we still issue Save/Restore + flush; just confirm
        // it doesn't panic and produces a finite byte sequence.
        assert!(out.len() < 64, "spurious bytes: {} bytes", out.len());
    }

    #[test]
    fn enabled_respects_env_var() {
        // Test the pure decision directly — no env mutation, so this can't
        // race with the URL-scan tests running in parallel.
        assert!(enabled_for(None));
        assert!(!enabled_for(Some("1")));
        assert!(!enabled_for(Some("true")));
        assert!(!enabled_for(Some(" on ")));
        assert!(enabled_for(Some("0")));
        assert!(enabled_for(Some("")));
    }
}
