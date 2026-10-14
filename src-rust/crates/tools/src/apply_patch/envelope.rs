// The `*** Begin Patch` envelope format used by OpenAI's Codex models.
//
// Ported from openai/codex `codex-rs/apply-patch` (parser.rs,
// streaming_parser.rs, seek_sequence.rs, file_update.rs, text_file.rs and
// invocation.rs), Copyright 2025 OpenAI, licensed under the Apache License,
// Version 2.0 (http://www.apache.org/licenses/LICENSE-2.0). Changes: a
// line-based parser in place of the streaming one, no environment ids,
// fuzzy (non-exact) context matches must be unique, a UTF-8 BOM is kept out
// of matching, and the heredoc form is recognised without tree-sitter.
//
// Grammar (codex-rs/core/assets/tools/apply_patch.lark):
//
//   start: begin_patch hunk+ end_patch
//   begin_patch: "*** Begin Patch" LF
//   end_patch: "*** End Patch" LF?
//   hunk: add_hunk | delete_hunk | update_hunk
//   add_hunk: "*** Add File: " filename LF add_line+
//   delete_hunk: "*** Delete File: " filename LF
//   update_hunk: "*** Update File: " filename LF change_move? change?
//   filename: /(.+)/
//   add_line: "+" /(.*)/ LF -> line
//   change_move: "*** Move to: " filename LF
//   change: (change_context | change_line)+ eof_line?
//   change_context: ("@@" | "@@ " /(.+)/) LF
//   change_line: ("+" | "-" | " ") /(.*)/ LF
//   eof_line: "*** End of File" LF

const BEGIN_PATCH_MARKER: &str = "*** Begin Patch";
const END_PATCH_MARKER: &str = "*** End Patch";
const ADD_FILE_MARKER: &str = "*** Add File: ";
const DELETE_FILE_MARKER: &str = "*** Delete File: ";
const UPDATE_FILE_MARKER: &str = "*** Update File: ";
const MOVE_TO_MARKER: &str = "*** Move to: ";
const EOF_MARKER: &str = "*** End of File";
const CHANGE_CONTEXT_MARKER: &str = "@@ ";
const EMPTY_CHANGE_CONTEXT_MARKER: &str = "@@";

/// One file operation in an envelope patch.
#[derive(Debug, PartialEq)]
pub(super) enum Hunk {
    Add { path: String, contents: String },
    Delete { path: String },
    Update { path: String, move_path: Option<String>, chunks: Vec<UpdateFileChunk> },
}

/// One `@@` section of an `*** Update File:` hunk.
#[derive(Debug, Default, PartialEq)]
pub(super) struct UpdateFileChunk {
    /// The `@@ <line>` anchor; `old_lines` must occur after it.
    pub(super) change_context: Option<String>,
    /// Lines to find (context and removed lines) ...
    pub(super) old_lines: Vec<String>,
    /// ... and what replaces them (context and added lines).
    pub(super) new_lines: Vec<String>,
    /// `(old, new)` indices of lines parsed as context, kept from the file
    /// as they are rather than rewritten.
    pub(super) context_line_indices: Vec<(usize, usize)>,
    /// `*** End of File`: `old_lines` must end the file.
    pub(super) is_end_of_file: bool,
}

impl UpdateFileChunk {
    fn push_context_line(&mut self, line: &str) {
        self.context_line_indices.push((self.old_lines.len(), self.new_lines.len()));
        self.old_lines.push(line.to_string());
        self.new_lines.push(line.to_string());
    }

    fn is_empty(&self) -> bool {
        self.old_lines.is_empty() && self.new_lines.is_empty()
    }
}

/// Whether `patch` is an envelope patch (possibly wrapped in a heredoc)
/// rather than a unified diff.
pub(super) fn is_envelope(patch: &str) -> bool {
    let mut lines = patch.trim().lines().map(str::trim);
    match lines.next() {
        Some(BEGIN_PATCH_MARKER) => true,
        Some("<<EOF" | "<<'EOF'" | "<<\"EOF\"") => lines.next() == Some(BEGIN_PATCH_MARKER),
        _ => false,
    }
}

fn invalid_patch(message: &str) -> String {
    format!("invalid patch: {}", message)
}

fn invalid_hunk(line_number: usize, message: String) -> String {
    format!("invalid hunk at line {}, {}", line_number, message)
}

fn unexpected_update_line(line_number: usize, line: &str) -> String {
    invalid_hunk(
        line_number,
        format!(
            "Unexpected line found in update hunk: '{}'. Every line should start with ' ' \
             (context line), '+' (added line), or '-' (removed line)",
            line
        ),
    )
}

/// Parse an envelope patch into hunks. Like codex's lenient mode, markers
/// may carry surrounding whitespace and the patch may be wrapped in a
/// `<<EOF` / `<<'EOF'` / `<<"EOF"` heredoc.
pub(super) fn parse_patch(patch: &str) -> Result<Vec<Hunk>, String> {
    let all: Vec<&str> = patch.trim().lines().collect();
    let boundaries_ok = |lines: &[&str]| -> Result<(), String> {
        match (lines.first().map(|l| l.trim()), lines.last().map(|l| l.trim())) {
            (Some(BEGIN_PATCH_MARKER), Some(END_PATCH_MARKER)) if lines.len() >= 2 => Ok(()),
            (Some(first), _) if first != BEGIN_PATCH_MARKER => Err(invalid_patch(
                "The first line of the patch must be '*** Begin Patch'",
            )),
            _ => Err(invalid_patch("The last line of the patch must be '*** End Patch'")),
        }
    };
    let (lines, offset) = match boundaries_ok(&all) {
        Ok(()) => (&all[..], 0),
        Err(original) => match all.as_slice() {
            [first, .., last]
                if matches!(*first, "<<EOF" | "<<'EOF'" | "<<\"EOF\"")
                    && last.ends_with("EOF")
                    && all.len() >= 4 =>
            {
                let inner = &all[1..all.len() - 1];
                boundaries_ok(inner)?;
                (inner, 1)
            }
            _ => return Err(original),
        },
    };

    let mut hunks: Vec<Hunk> = Vec::new();
    // Line number of the open `*** Update File:` header, if any.
    let mut update_header: Option<usize> = None;
    let body = &lines[1..lines.len() - 1];

    // Rejects an update hunk with no chunks or an empty last chunk before
    // the next header or the end of the patch.
    let close_update = |hunks: &[Hunk], header: Option<usize>, line_number: usize, line: &str| {
        let (Some(header), Some(Hunk::Update { path, chunks, .. })) = (header, hunks.last()) else {
            return Ok(());
        };
        match chunks.last() {
            None => Err(invalid_hunk(header, format!("Update file hunk for path '{}' is empty", path))),
            Some(chunk) if chunk.is_empty() && line == END_PATCH_MARKER => Err(invalid_hunk(
                line_number,
                "Update hunk does not contain any lines".to_string(),
            )),
            Some(chunk) if chunk.is_empty() => Err(unexpected_update_line(line_number, line)),
            Some(_) => Ok(()),
        }
    };

    for (idx, raw) in body.iter().enumerate() {
        // 1-based line number in the text as given (heredoc line included).
        let line_number = idx + 2 + offset;
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        // Inside an update hunk only trailing whitespace is ignored, so a
        // context line such as ` *** Add File: x` stays a context line.
        let trimmed = match hunks.last() {
            Some(Hunk::Update { .. }) => line.trim_end(),
            _ => line.trim(),
        };

        // File headers end whatever hunk is open.
        let header = if let Some(path) = trimmed.strip_prefix(ADD_FILE_MARKER) {
            Some(Hunk::Add { path: path.to_string(), contents: String::new() })
        } else if let Some(path) = trimmed.strip_prefix(DELETE_FILE_MARKER) {
            Some(Hunk::Delete { path: path.to_string() })
        } else {
            trimmed.strip_prefix(UPDATE_FILE_MARKER).map(|path| Hunk::Update {
                path: path.to_string(),
                move_path: None,
                chunks: Vec::new(),
            })
        };
        if let Some(hunk) = header {
            close_update(&hunks, update_header, line_number, trimmed)?;
            update_header = matches!(hunk, Hunk::Update { .. }).then_some(line_number);
            hunks.push(hunk);
            continue;
        }

        let not_a_header = || {
            invalid_hunk(
                line_number,
                format!(
                    "'{}' is not a valid hunk header. Valid hunk headers: '*** Add File: {{path}}', \
                     '*** Delete File: {{path}}', '*** Update File: {{path}}'",
                    trimmed
                ),
            )
        };
        match hunks.last_mut() {
            None | Some(Hunk::Delete { .. }) => return Err(not_a_header()),
            Some(Hunk::Add { contents, .. }) => match line.strip_prefix('+') {
                Some(added) => {
                    contents.push_str(added);
                    contents.push('\n');
                }
                None => return Err(not_a_header()),
            },
            Some(Hunk::Update { move_path, chunks, .. }) => {
                let update_line = line.trim_end();
                if chunks.last().is_some_and(|chunk| chunk.is_end_of_file) {
                    if update_line.is_empty() {
                        continue;
                    }
                    if update_line != EMPTY_CHANGE_CONTEXT_MARKER
                        && !update_line.starts_with(CHANGE_CONTEXT_MARKER)
                    {
                        return Err(invalid_hunk(
                            line_number,
                            format!("Expected update hunk to start with a @@ context marker, got: '{}'", line),
                        ));
                    }
                }
                if chunks.is_empty() && move_path.is_none() {
                    if let Some(dest) = update_line.strip_prefix(MOVE_TO_MARKER) {
                        *move_path = Some(dest.to_string());
                        continue;
                    }
                }
                let context = if update_line == EMPTY_CHANGE_CONTEXT_MARKER {
                    Some(None)
                } else {
                    update_line.strip_prefix(CHANGE_CONTEXT_MARKER).map(|ctx| Some(ctx.to_string()))
                };
                if let Some(change_context) = context {
                    if chunks.last().is_some_and(UpdateFileChunk::is_empty) {
                        return Err(unexpected_update_line(line_number, line));
                    }
                    chunks.push(UpdateFileChunk { change_context, ..UpdateFileChunk::default() });
                    continue;
                }
                if update_line == EOF_MARKER {
                    match chunks.last_mut() {
                        Some(chunk) if !chunk.is_empty() => chunk.is_end_of_file = true,
                        _ => {
                            return Err(invalid_hunk(
                                line_number,
                                "Update hunk does not contain any lines".to_string(),
                            ))
                        }
                    }
                    continue;
                }
                let (kind, text) = match line.chars().next() {
                    // An empty line is an empty context line whose space was stripped.
                    None => (' ', ""),
                    Some(c @ (' ' | '+' | '-')) => (c, &line[1..]),
                    Some(_) => {
                        if chunks.last().is_some_and(|chunk| !chunk.is_empty()) {
                            return Err(invalid_hunk(
                                line_number,
                                format!("Expected update hunk to start with a @@ context marker, got: '{}'", line),
                            ));
                        }
                        return Err(unexpected_update_line(line_number, line));
                    }
                };
                if chunks.is_empty() {
                    chunks.push(UpdateFileChunk::default());
                }
                if let Some(chunk) = chunks.last_mut() {
                    match kind {
                        ' ' => chunk.push_context_line(text),
                        '+' => chunk.new_lines.push(text.to_string()),
                        _ => chunk.old_lines.push(text.to_string()),
                    }
                }
            }
        }
    }
    close_update(&hunks, update_header, lines.len() + offset, END_PATCH_MARKER)?;
    Ok(hunks)
}

// ---------------------------------------------------------------------------
// Locating chunks
// ---------------------------------------------------------------------------

/// Outcome of [`seek_sequence`].
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Seek {
    Found(usize),
    Missing,
    /// Only a fuzzy pass matched, and it matched more than one place.
    Ambiguous { count: usize, ignoring: &'static str },
}

/// Map typographic punctuation and odd spaces to ASCII after trimming, so a
/// patch written with ASCII matches source that uses Unicode dashes/quotes.
fn normalise(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| match c {
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
            | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

/// Find `pattern` in `lines` at or after `start` in four passes of
/// decreasing strictness: exact, ignoring trailing whitespace, ignoring
/// leading and trailing whitespace, then with Unicode punctuation folded to
/// ASCII. With `eof` the search starts where the pattern would end the file.
///
/// The exact pass takes the first match, as codex does (the forward cursor
/// and `@@` anchors disambiguate). A fuzzy pass that matches more than one
/// place is reported as ambiguous instead of guessing.
pub(super) fn seek_sequence(lines: &[String], pattern: &[String], start: usize, eof: bool) -> Seek {
    if pattern.is_empty() {
        return Seek::Found(start);
    }
    let Some(last_start) = lines.len().checked_sub(pattern.len()) else {
        return Seek::Missing;
    };
    let search_start = if eof { last_start.max(start) } else { start };
    let candidates = search_start..=last_start;

    if let Some(i) = candidates.clone().find(|&i| lines[i..i + pattern.len()] == *pattern) {
        return Seek::Found(i);
    }

    let passes: [(&'static str, fn(&str, &str) -> bool); 3] = [
        ("trailing whitespace", |a, b| a.trim_end() == b.trim_end()),
        ("leading and trailing whitespace", |a, b| a.trim() == b.trim()),
        ("whitespace and Unicode punctuation", |a, b| normalise(a) == normalise(b)),
    ];
    for (ignoring, same) in passes {
        let mut found = candidates
            .clone()
            .filter(|&i| pattern.iter().enumerate().all(|(p, pat)| same(&lines[i + p], pat)));
        if let Some(first) = found.next() {
            let others = found.count();
            return if others == 0 {
                Seek::Found(first)
            } else {
                Seek::Ambiguous { count: others + 1, ignoring }
            };
        }
    }
    Seek::Missing
}

/// `(start index, old line count, new lines)`.
type Replacement = (usize, usize, Vec<String>);

/// Work out the line replacements that apply `chunks` to `original_lines`,
/// moving a cursor forward through the file chunk by chunk.
fn compute_replacements(
    original_lines: &[String],
    path: &str,
    chunks: &[UpdateFileChunk],
) -> Result<Vec<Replacement>, String> {
    let mut replacements: Vec<Replacement> = Vec::new();
    let mut line_index = 0usize;

    for chunk in chunks {
        if let Some(ctx_line) = &chunk.change_context {
            match seek_sequence(original_lines, std::slice::from_ref(ctx_line), line_index, false) {
                Seek::Found(idx) => line_index = idx + 1,
                Seek::Missing => return Err(format!("Failed to find context '{}' in {}", ctx_line, path)),
                Seek::Ambiguous { count, ignoring } => {
                    return Err(format!(
                        "Context '{}' matches {} lines in {} only when ignoring {}; copy the \
                         line exactly or add an earlier @@ anchor",
                        ctx_line, count, path, ignoring
                    ));
                }
            }
        }

        if chunk.old_lines.is_empty() {
            // A pure addition goes at the end of the file, as in codex.
            replacements.push((original_lines.len(), 0, chunk.new_lines.clone()));
            continue;
        }

        let mut pattern: &[String] = &chunk.old_lines;
        let mut new_slice: &[String] = &chunk.new_lines;
        let mut found = seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);

        // A trailing empty old line stands for the file's final newline, which
        // is not a line of its own here; retry without it.
        if found == Seek::Missing && pattern.last().is_some_and(String::is_empty) {
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(String::is_empty) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }
            found = seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);
        }

        let start_idx = match found {
            Seek::Found(start_idx) => start_idx,
            Seek::Missing => {
                return Err(format!(
                    "Failed to find expected lines in {}:\n{}",
                    path,
                    chunk.old_lines.join("\n")
                ));
            }
            Seek::Ambiguous { count, ignoring } => {
                return Err(format!(
                    "The expected lines match {} places in {} only when ignoring {}; copy them \
                     exactly or add context lines or an @@ anchor so they match once:\n{}",
                    count,
                    path,
                    ignoring,
                    chunk.old_lines.join("\n")
                ));
            }
        };

        // Context lines stay as they are in the file (exact text and line
        // endings); only the runs between them are replaced.
        let mut old_start = 0;
        let mut new_start = 0;
        for &(old_context, new_context) in &chunk.context_line_indices {
            if old_context >= pattern.len() || new_context >= new_slice.len() {
                break;
            }
            if old_start != old_context || new_start != new_context {
                replacements.push((
                    start_idx + old_start,
                    old_context - old_start,
                    new_slice[new_start..new_context].to_vec(),
                ));
            }
            old_start = old_context + 1;
            new_start = new_context + 1;
        }
        if old_start != pattern.len() || new_start != new_slice.len() {
            replacements.push((
                start_idx + old_start,
                pattern.len() - old_start,
                new_slice[new_start..].to_vec(),
            ));
        }
        line_index = start_idx + pattern.len();
    }

    replacements.sort_by_key(|(index, _, _)| *index);
    Ok(replacements)
}

// ---------------------------------------------------------------------------
// Per-line line endings
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum LineEnding {
    Lf,
    CrLf,
    Cr,
}

impl LineEnding {
    fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
            Self::Cr => "\r",
        }
    }
}

struct SourceLine {
    text: String,
    ending: Option<LineEnding>,
}

/// File content split into lines that keep their own endings, so untouched
/// lines of a mixed-ending file come back byte for byte.
struct SourceFile {
    lines: Vec<SourceLine>,
    /// The first ending in the file (LF if none): used for inserted lines.
    preferred_ending: LineEnding,
}

impl SourceFile {
    fn parse(contents: &str) -> Self {
        let bytes = contents.as_bytes();
        let mut lines = Vec::new();
        let mut preferred_ending = None;
        let mut line_start = 0;
        let mut cursor = 0;
        while cursor < bytes.len() {
            let (ending, len) = match bytes[cursor] {
                b'\r' if bytes.get(cursor + 1) == Some(&b'\n') => (LineEnding::CrLf, 2),
                b'\r' => (LineEnding::Cr, 1),
                b'\n' => (LineEnding::Lf, 1),
                _ => {
                    cursor += 1;
                    continue;
                }
            };
            preferred_ending.get_or_insert(ending);
            lines.push(SourceLine { text: contents[line_start..cursor].to_string(), ending: Some(ending) });
            cursor += len;
            line_start = cursor;
        }
        if line_start < contents.len() {
            lines.push(SourceLine { text: contents[line_start..].to_string(), ending: None });
        }
        Self { lines, preferred_ending: preferred_ending.unwrap_or(LineEnding::Lf) }
    }

    fn line_texts(&self) -> Vec<String> {
        self.lines.iter().map(|line| line.text.clone()).collect()
    }

    /// Apply sorted, non-overlapping replacements. Inserted lines take the
    /// preferred ending, and every line ends with a newline afterwards (an
    /// update always leaves a trailing newline, as in codex).
    fn apply_replacements(&mut self, replacements: &[Replacement]) {
        let mut source_lines = std::mem::take(&mut self.lines).into_iter();
        let mut new_lines = Vec::new();
        let mut source_index = 0;
        for (start_idx, old_len, new_segment) in replacements {
            new_lines.extend(source_lines.by_ref().take(start_idx.saturating_sub(source_index)));
            source_lines.by_ref().take(*old_len).for_each(drop);
            new_lines.extend(new_segment.iter().map(|text| SourceLine {
                text: text.clone(),
                ending: Some(self.preferred_ending),
            }));
            source_index = start_idx + old_len;
        }
        new_lines.extend(source_lines);
        for line in &mut new_lines {
            line.ending.get_or_insert(self.preferred_ending);
        }
        self.lines = new_lines;
    }

    fn into_contents(self) -> String {
        let mut contents = String::new();
        for line in self.lines {
            contents.push_str(&line.text);
            if let Some(ending) = line.ending {
                contents.push_str(ending.as_str());
            }
        }
        contents
    }
}

/// Apply an update hunk's chunks to `original`, the current content of the
/// file the patch calls `path`. A UTF-8 BOM is kept out of matching and put
/// back afterwards.
pub(super) fn apply_chunks(original: &str, path: &str, chunks: &[UpdateFileChunk]) -> Result<String, String> {
    let (bom, content) = match original.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", original),
    };
    let mut source = SourceFile::parse(content);
    let replacements = compute_replacements(&source.line_texts(), path, chunks)?;
    source.apply_replacements(&replacements);
    Ok(format!("{}{}", bom, source.into_contents()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn to_vec(strings: &[&str]) -> Vec<String> {
        strings.iter().map(|s| s.to_string()).collect()
    }

    // --- seek_sequence (codex seek_sequence.rs tests, plus ambiguity) ---

    #[test]
    fn seek_exact_match_finds_sequence() {
        let lines = to_vec(&["foo", "bar", "baz"]);
        assert_eq!(seek_sequence(&lines, &to_vec(&["bar", "baz"]), 0, false), Seek::Found(1));
    }

    #[test]
    fn seek_rstrip_match_ignores_trailing_whitespace() {
        let lines = to_vec(&["foo   ", "bar\t\t"]);
        assert_eq!(seek_sequence(&lines, &to_vec(&["foo", "bar"]), 0, false), Seek::Found(0));
    }

    #[test]
    fn seek_trim_match_ignores_leading_and_trailing_whitespace() {
        let lines = to_vec(&["    foo   ", "   bar\t"]);
        assert_eq!(seek_sequence(&lines, &to_vec(&["foo", "bar"]), 0, false), Seek::Found(0));
    }

    #[test]
    fn seek_unicode_pass_folds_punctuation() {
        let lines = to_vec(&["a \u{2013} b \u{201C}q\u{201D}"]);
        assert_eq!(seek_sequence(&lines, &to_vec(&["a - b \"q\""]), 0, false), Seek::Found(0));
    }

    #[test]
    fn seek_pattern_longer_than_input_returns_missing() {
        let lines = to_vec(&["just one line"]);
        assert_eq!(seek_sequence(&lines, &to_vec(&["too", "many", "lines"]), 0, false), Seek::Missing);
    }

    #[test]
    fn seek_exact_takes_first_match_after_cursor() {
        let lines = to_vec(&["x", "x", "x"]);
        assert_eq!(seek_sequence(&lines, &to_vec(&["x"]), 1, false), Seek::Found(1));
    }

    #[test]
    fn seek_fuzzy_match_in_two_places_is_ambiguous() {
        let lines = to_vec(&["  }", "x", "    }"]);
        assert_eq!(
            seek_sequence(&lines, &to_vec(&["}"]), 0, false),
            Seek::Ambiguous { count: 2, ignoring: "leading and trailing whitespace" }
        );
        // From a cursor past the first one, the match is unique again.
        assert_eq!(seek_sequence(&lines, &to_vec(&["}"]), 1, false), Seek::Found(2));
    }

    #[test]
    fn seek_eof_starts_at_the_end() {
        let lines = to_vec(&["a", "b", "a", "b"]);
        assert_eq!(seek_sequence(&lines, &to_vec(&["a", "b"]), 0, true), Seek::Found(2));
    }

    // --- parser (codex parser.rs tests) ---

    #[test]
    fn parse_rejects_bad_boundaries() {
        assert_eq!(
            parse_patch("bad"),
            Err("invalid patch: The first line of the patch must be '*** Begin Patch'".to_string())
        );
        assert_eq!(
            parse_patch("*** Begin Patch\nbad"),
            Err("invalid patch: The last line of the patch must be '*** End Patch'".to_string())
        );
    }

    #[test]
    fn parse_all_hunk_kinds() {
        let patch = "*** Begin Patch\n\
                     *** Add File: path/add.py\n\
                     +abc\n\
                     +def\n\
                     *** Delete File: path/delete.py\n\
                     *** Update File: path/update.py\n\
                     *** Move to: path/update2.py\n\
                     @@ def f():\n\
                     -    pass\n\
                     +    return 123\n\
                     *** End Patch";
        assert_eq!(
            parse_patch(patch).unwrap(),
            vec![
                Hunk::Add { path: "path/add.py".into(), contents: "abc\ndef\n".into() },
                Hunk::Delete { path: "path/delete.py".into() },
                Hunk::Update {
                    path: "path/update.py".into(),
                    move_path: Some("path/update2.py".into()),
                    chunks: vec![UpdateFileChunk {
                        change_context: Some("def f():".into()),
                        old_lines: to_vec(&["    pass"]),
                        new_lines: to_vec(&["    return 123"]),
                        context_line_indices: vec![],
                        is_end_of_file: false,
                    }],
                },
            ]
        );
    }

    #[test]
    fn parse_empty_update_hunk_is_an_error() {
        assert_eq!(
            parse_patch("*** Begin Patch\n*** Update File: test.py\n*** End Patch"),
            Err("invalid hunk at line 2, Update file hunk for path 'test.py' is empty".to_string())
        );
    }

    #[test]
    fn parse_update_without_leading_at_at() {
        let patch = "*** Begin Patch\n*** Update File: file2.py\n import foo\n+bar\n*** End Patch";
        assert_eq!(
            parse_patch(patch).unwrap(),
            vec![Hunk::Update {
                path: "file2.py".into(),
                move_path: None,
                chunks: vec![UpdateFileChunk {
                    change_context: None,
                    old_lines: to_vec(&["import foo"]),
                    new_lines: to_vec(&["import foo", "bar"]),
                    context_line_indices: vec![(0, 0)],
                    is_end_of_file: false,
                }],
            }]
        );
    }

    #[test]
    fn parse_end_of_file_marker() {
        let patch = "*** Begin Patch\n*** Update File: file.txt\n@@\n+quux\n*** End of File\n\n*** End Patch";
        let Hunk::Update { chunks, .. } = &parse_patch(patch).unwrap()[0] else {
            panic!("expected an update hunk");
        };
        assert!(chunks[0].is_end_of_file);
        assert_eq!(chunks[0].new_lines, to_vec(&["quux"]));
    }

    #[test]
    fn parse_heredoc_wrapped_patch() {
        let inner = "*** Begin Patch\n*** Add File: a.txt\n+hi\n*** End Patch";
        for wrapper in ["<<EOF", "<<'EOF'", "<<\"EOF\""] {
            let patch = format!("{}\n{}\nEOF\n", wrapper, inner);
            assert!(is_envelope(&patch));
            assert_eq!(
                parse_patch(&patch).unwrap(),
                vec![Hunk::Add { path: "a.txt".into(), contents: "hi\n".into() }]
            );
        }
        assert!(parse_patch(&format!("<<\"EOF'\n{}\nEOF\n", inner)).is_err());
    }

    #[test]
    fn parse_invalid_hunk_header() {
        assert_eq!(
            parse_patch("*** Begin Patch\n*** Frobnicate File: foo\n*** End Patch"),
            Err("invalid hunk at line 2, '*** Frobnicate File: foo' is not a valid hunk header. \
                 Valid hunk headers: '*** Add File: {path}', '*** Delete File: {path}', \
                 '*** Update File: {path}'"
                .to_string())
        );
    }
}
