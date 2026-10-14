//! Text truncation utilities — mirrors src/utils/truncate.ts

/// Truncate `text` to at most `max_chars` characters.
/// If truncated, appends `… (truncated)`.
pub fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    // Find a safe char boundary
    let mut end = max_chars;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… (truncated)", &text[..end])
}

/// Truncate a list of lines to at most `max_lines`.
/// If truncated, appends a `"… N more lines"` indicator.
pub fn truncate_lines(lines: &[String], max_lines: usize) -> (Vec<String>, bool) {
    if lines.len() <= max_lines {
        return (lines.to_vec(), false);
    }
    let mut out = lines[..max_lines].to_vec();
    let remaining = lines.len() - max_lines;
    out.push(format!("… {} more line{}", remaining, if remaining == 1 { "" } else { "s" }));
    (out, true)
}

/// Truncate tool output to a safe display length.
/// Returns `(truncated_text, was_truncated)`.
pub fn truncate_tool_output(text: &str, max_chars: usize) -> (String, bool) {
    if text.len() <= max_chars {
        return (text.to_string(), false);
    }
    let mut end = max_chars;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (
        format!("{}… [{} chars truncated]", &text[..end], text.len() - end),
        true,
    )
}

/// Keep the first and last `max_bytes / 2` bytes of `text` around a marker
/// naming how many bytes were dropped. Both cuts land on char boundaries, so
/// multibyte output at the cut point never panics (#442). Returns `text`
/// unchanged when it already fits.
pub fn truncate_middle(text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let half = max_bytes / 2;
    let head_end = text.floor_char_boundary(half);
    let tail_start = text.ceil_char_boundary(text.len() - half);
    format!(
        "{}\n\n... ({} bytes truncated) ...\n\n{}",
        &text[..head_end],
        tail_start - head_end,
        &text[tail_start..]
    )
}

/// Line cap for each of the head and tail windows of bounded tool output (#442).
pub const OUTPUT_WINDOW_LINES: usize = 2000;
/// Byte cap for each of the head and tail windows of bounded tool output (#442).
pub const OUTPUT_WINDOW_BYTES: usize = 50 * 1024;

/// Longest prefix of `text` holding at most `max_lines` lines and `max_bytes`
/// bytes, ending on a char boundary.
pub fn head_window(text: &str, max_lines: usize, max_bytes: usize) -> &str {
    let Some(nth) = max_lines.checked_sub(1) else {
        return "";
    };
    let head = &text[..text.floor_char_boundary(max_bytes)];
    match head.match_indices('\n').nth(nth) {
        Some((i, _)) => &head[..i],
        None => head,
    }
}

/// Longest suffix of `text` holding at most `max_lines` lines and `max_bytes`
/// bytes, starting on a char boundary. A trailing newline does not count as
/// the start of an extra line.
pub fn tail_window(text: &str, max_lines: usize, max_bytes: usize) -> &str {
    let Some(nth) = max_lines.checked_sub(1) else {
        return "";
    };
    let tail = &text[text.ceil_char_boundary(text.len().saturating_sub(max_bytes))..];
    let body = tail.strip_suffix('\n').unwrap_or(tail);
    match body.rmatch_indices('\n').nth(nth) {
        Some((i, _)) => &tail[i + 1..],
        None => tail,
    }
}

/// Join a head and a tail window around a marker naming what was left out.
pub fn join_head_tail(head: &str, tail: &str, omitted: &str) -> String {
    format!("{head}\n\n... ({omitted} omitted) ...\n\n{tail}")
}

/// `None` when `text` fits in one head plus one tail window
/// ([`OUTPUT_WINDOW_LINES`] / [`OUTPUT_WINDOW_BYTES`] each); otherwise the head
/// window, an omission marker and the tail window (#442).
pub fn bound_head_tail(text: &str) -> Option<String> {
    let lines = text.bytes().filter(|&b| b == b'\n').count() + 1;
    if text.len() <= 2 * OUTPUT_WINDOW_BYTES && lines <= 2 * OUTPUT_WINDOW_LINES {
        return None;
    }
    let head = head_window(text, OUTPUT_WINDOW_LINES, OUTPUT_WINDOW_BYTES);
    let tail = tail_window(&text[head.len()..], OUTPUT_WINDOW_LINES, OUTPUT_WINDOW_BYTES);
    let omitted = text.len() - head.len() - tail.len();
    Some(join_head_tail(head, tail, &format!("{omitted} bytes")))
}

/// Truncate a file path for display, keeping the filename and shortening the directory.
pub fn truncate_path(path: &str, max_chars: usize) -> String {
    if path.len() <= max_chars {
        return path.to_string();
    }
    let filename = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path);
    if filename.len() >= max_chars {
        return filename.to_string();
    }
    let prefix_len = max_chars - filename.len() - 4; // 4 for "…/"
    let dir = std::path::Path::new(path)
        .parent()
        .and_then(|p| p.to_str())
        .unwrap_or("");
    if dir.len() <= prefix_len {
        return path.to_string();
    }
    format!("…/{}/{}", &dir[dir.len() - prefix_len..], filename)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_text_short() {
        assert_eq!(truncate_text("hello", 10), "hello");
    }

    #[test]
    fn truncate_text_long() {
        let t = truncate_text("hello world", 5);
        assert!(t.starts_with("hello"));
        assert!(t.contains("truncated"));
    }

    #[test]
    fn truncate_middle_short_text_is_unchanged() {
        assert_eq!(truncate_middle("hello".to_string(), 10), "hello");
    }

    /// #442: a multibyte char straddling either cut must not panic, and both
    /// halves must stay valid UTF-8 slices of the input.
    #[test]
    fn truncate_middle_multibyte_at_cut_does_not_panic() {
        // 'é' is 2 bytes and '€' is 3, so every byte offset is hit by a cut
        // somewhere across these lengths.
        for max in 1..40 {
            let text = "é€".repeat(20);
            let out = truncate_middle(text.clone(), max);
            assert!(out.contains("bytes truncated"), "max={max}: {out}");
            let (head, rest) = out.split_once("\n\n... (").unwrap();
            let (_, tail) = rest.split_once(") ...\n\n").unwrap();
            assert!(text.starts_with(head));
            assert!(text.ends_with(tail));
            assert!(head.len() <= max / 2 && tail.len() <= max / 2);
        }
    }

    #[test]
    fn head_and_tail_windows_respect_line_caps() {
        let text = "1\n2\n3\n4\n5\n";
        assert_eq!(head_window(text, 2, 100), "1\n2");
        assert_eq!(tail_window(text, 2, 100), "4\n5\n");
        assert_eq!(head_window(text, 0, 100), "");
        assert_eq!(tail_window(text, 0, 100), "");
        assert_eq!(head_window(text, 99, 100), text);
        assert_eq!(tail_window(text, 99, 100), text);
    }

    #[test]
    fn head_and_tail_windows_cut_on_char_boundaries() {
        let text = "é".repeat(10);
        for max in 0..25 {
            let head = head_window(&text, 99, max);
            let tail = tail_window(&text, 99, max);
            assert!(head.len() <= max && tail.len() <= max);
            assert!(text.starts_with(head) && text.ends_with(tail));
        }
    }

    #[test]
    fn bound_head_tail_keeps_small_text_and_cuts_large_text() {
        assert_eq!(bound_head_tail("short"), None);

        let lines: Vec<String> = (0..10_000).map(|i| format!("line {i}")).collect();
        let text = lines.join("\n");
        let out = bound_head_tail(&text).unwrap();
        assert!(out.starts_with("line 0\n"));
        assert!(out.ends_with("line 9999"));
        assert!(out.contains("bytes omitted"));
        assert!(!out.contains("line 5000\n"));
        let kept = out.lines().filter(|l| l.starts_with("line ")).count();
        assert_eq!(kept, 2 * OUTPUT_WINDOW_LINES);
    }

    #[test]
    fn truncate_lines_over_limit() {
        let lines: Vec<String> = (0..10).map(|i| format!("line {}", i)).collect();
        let (out, truncated) = truncate_lines(&lines, 5);
        assert!(truncated);
        assert_eq!(out.len(), 6); // 5 lines + 1 indicator
        assert!(out[5].contains("5 more"));
    }
}
