//! Line-ending detection and preservation for the file-editing tools.
//!
//! Issue #225: the edit tools normalized `\r\n` -> `\n` for matching and then
//! wrote the LF-normalized content back, silently rewriting *every* line ending
//! of a CRLF file to LF. That produces a massive spurious diff and corrupts
//! files that must stay CRLF (this repo's own `crates/tui/src/lib.rs` is
//! intentionally CRLF).
//!
//! These helpers let a tool match/replace on an LF-normalized view while
//! writing back with the file's original line endings, so only the lines an
//! edit actually changes ever differ.

/// The dominant line ending of a text file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    /// Unix `\n`.
    Lf,
    /// Windows `\r\n`.
    Crlf,
}

impl LineEnding {
    /// Detect the dominant line ending of `content`.
    ///
    /// CRLF is reported only when it is the majority of the file's line
    /// endings, so a predominantly-CRLF file stays CRLF and a predominantly-LF
    /// file (or one with only a stray `\r\n`) stays LF. A file with no newline
    /// at all defaults to [`LineEnding::Lf`].
    pub fn detect(content: &str) -> LineEnding {
        let crlf = content.matches("\r\n").count();
        if crlf == 0 {
            return LineEnding::Lf;
        }
        // Every `\r\n` contributes one `\n`, so bare LF endings are the total
        // `\n` count minus the CRLF count.
        let bare_lf = content.matches('\n').count() - crlf;
        if crlf >= bare_lf {
            LineEnding::Crlf
        } else {
            LineEnding::Lf
        }
    }

    /// The byte sequence for this line ending.
    pub fn as_str(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::Crlf => "\r\n",
        }
    }

    /// Render LF-normalized `content` with this line ending.
    ///
    /// `content` must already use `\n` for every line ending (as produced by
    /// replacing `\r\n` with `\n`). For [`LineEnding::Lf`] this is a no-op; for
    /// [`LineEnding::Crlf`] every `\n` becomes `\r\n`.
    pub fn apply(self, content: &str) -> String {
        match self {
            LineEnding::Lf => content.to_string(),
            LineEnding::Crlf => content.replace('\n', "\r\n"),
        }
    }
}

/// Replace `old_norm` with `new_norm` inside `original`, preserving the exact
/// bytes — and therefore the line endings — of every region that is NOT
/// replaced.
///
/// Matching is line-ending agnostic: it runs against an LF-normalized view of
/// `original`, so an `old_norm` that uses `\n` still matches a region stored
/// with `\r\n`. `old_norm` and `new_norm` must already be LF-normalized; the
/// replacement text is re-rendered with `eol` so inserted lines adopt the
/// file's dominant line ending. Untouched regions keep their original bytes
/// verbatim, which is what makes a mixed-EOL file safe to edit (nothing outside
/// the replaced span is mass-converted).
///
/// Returns `(new_content, replacements_made)`. When `replace_all` is false only
/// the first occurrence is replaced.
pub(crate) fn replace_preserving_eol(
    original: &str,
    old_norm: &str,
    new_norm: &str,
    eol: LineEnding,
    replace_all: bool,
) -> (String, usize) {
    let orig = original.as_bytes();

    // Build an LF-normalized byte view of `original`, together with a map from
    // each normalized-byte index to its starting offset in `original`. A `\r\n`
    // collapses to a single `\n` that maps to the `\r`'s offset.
    let mut norm: Vec<u8> = Vec::with_capacity(orig.len());
    let mut map: Vec<usize> = Vec::with_capacity(orig.len() + 1);
    let mut i = 0;
    while i < orig.len() {
        map.push(i);
        if orig[i] == b'\r' && i + 1 < orig.len() && orig[i + 1] == b'\n' {
            norm.push(b'\n');
            i += 2;
        } else {
            norm.push(orig[i]);
            i += 1;
        }
    }
    map.push(orig.len()); // sentinel: index for a match that ends at EOF

    let old_b = old_norm.as_bytes();
    let new_rendered = eol.apply(new_norm);
    let new_b = new_rendered.as_bytes();

    let mut result: Vec<u8> = Vec::with_capacity(orig.len());
    let mut copied = 0usize; // bytes of `orig` already emitted
    let mut search_from = 0usize; // index into `norm`
    let mut count = 0usize;

    while let Some(rel) = find_subslice(&norm[search_from..], old_b) {
        let start = search_from + rel; // norm index of match start
        let end = start + old_b.len(); // norm index just past the match

        // Map the normalized match span back onto original byte offsets. UTF-8
        // is self-synchronizing, so a valid needle can only match on a char
        // boundary — these offsets always land on boundaries.
        let orig_start = map[start];
        let orig_end = map[end];

        result.extend_from_slice(&orig[copied..orig_start]);
        result.extend_from_slice(new_b);
        copied = orig_end;
        count += 1;
        search_from = end;

        if !replace_all {
            break;
        }
    }

    result.extend_from_slice(&orig[copied..]);

    // Safe: we only drop a `\r` that precedes a `\n` and splice in valid UTF-8.
    // The lossy fallback is purely defensive and should be unreachable.
    let new_content = match String::from_utf8(result) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };
    (new_content, count)
}

/// Return the index of the first occurrence of `needle` within `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_pure_lf() {
        assert_eq!(LineEnding::detect("a\nb\nc\n"), LineEnding::Lf);
        assert_eq!(LineEnding::detect("no newline at all"), LineEnding::Lf);
        assert_eq!(LineEnding::detect(""), LineEnding::Lf);
    }

    #[test]
    fn detect_pure_crlf() {
        assert_eq!(LineEnding::detect("a\r\nb\r\nc\r\n"), LineEnding::Crlf);
    }

    #[test]
    fn detect_mixed_uses_majority() {
        // Majority CRLF (2 CRLF vs 1 bare LF) -> Crlf.
        assert_eq!(LineEnding::detect("a\r\nb\r\nc\nd"), LineEnding::Crlf);
        // Majority LF (1 CRLF vs 2 bare LF) -> Lf.
        assert_eq!(LineEnding::detect("a\r\nb\nc\nd"), LineEnding::Lf);
    }

    #[test]
    fn apply_roundtrips() {
        assert_eq!(LineEnding::Lf.apply("a\nb\n"), "a\nb\n");
        assert_eq!(LineEnding::Crlf.apply("a\nb\n"), "a\r\nb\r\n");
    }

    #[test]
    fn replace_lf_stays_lf() {
        let (out, n) =
            replace_preserving_eol("a\nb\nc\n", "b", "B", LineEnding::Lf, false);
        assert_eq!(out, "a\nB\nc\n");
        assert_eq!(n, 1);
    }

    #[test]
    fn replace_crlf_keeps_crlf_and_only_target_changes() {
        // LF-normalized old_string matches a CRLF region; every other EOL stays.
        let (out, n) =
            replace_preserving_eol("a\r\nb\r\nc\r\n", "b", "B", LineEnding::Crlf, false);
        assert_eq!(out, "a\r\nB\r\nc\r\n");
        assert_eq!(n, 1);
    }

    #[test]
    fn replace_crlf_multiline_new_text_gets_crlf() {
        let (out, _) = replace_preserving_eol(
            "one\r\ntwo\r\nthree\r\n",
            "two",
            "TWO\nEXTRA",
            LineEnding::Crlf,
            false,
        );
        assert_eq!(out, "one\r\nTWO\r\nEXTRA\r\nthree\r\n");
    }

    #[test]
    fn replace_all_crlf() {
        let (out, n) =
            replace_preserving_eol("x\r\nx\r\nx\r\n", "x", "y", LineEnding::Crlf, true);
        assert_eq!(out, "y\r\ny\r\ny\r\n");
        assert_eq!(n, 3);
    }

    #[test]
    fn mixed_file_leaves_untouched_eols_byte_identical() {
        // A deliberately mixed file: CRLF, then LF, then CRLF. Editing the LF
        // line must not flip the surrounding CRLF endings (and vice versa).
        let original = "a\r\nb\nc\r\n";
        let eol = LineEnding::detect(original); // Crlf is the majority (2 vs 1)
        let (out, n) = replace_preserving_eol(original, "b", "B", eol, false);
        // Only "b" -> "B"; the trailing `\n` after b and the CRLFs are intact.
        assert_eq!(out, "a\r\nB\nc\r\n");
        assert_eq!(n, 1);
    }
}
