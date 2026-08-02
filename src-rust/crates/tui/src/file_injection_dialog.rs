use std::path::PathBuf;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph, Widget, Wrap};
use ratatui::Frame;

use crate::file_injection::AtFileIssue;
use crate::image_paste::PastedImage;

/// Outcome of the file injection dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileInjectionOutcome {
    /// Inject all files, ignoring size/binary limits.
    InjectAll,
    /// Abort (restore input to prompt for editing).
    Abort,
}

/// State for the file injection warning dialog.
/// Shown when oversized or binary files are detected in @refs.
#[derive(Debug, Clone)]
pub struct FileInjectionDialogState {
    /// Whether the dialog is currently visible.
    pub visible: bool,
    /// Stashed input text (taken from prompt, must be re-set or sent).
    pub pending_input: Option<String>,
    /// Stashed image attachments at submit time.
    pub pending_imgs: Vec<PastedImage>,
    /// Files that exceeded limits or had issues: (path, size_kb, issue).
    pub oversized: Vec<(String, usize, AtFileIssue)>,
    /// Currently selected option: 0 = Allow, 1 = Abort.
    pub selected: usize,
    /// Size limit in KB, for display in the dialog.
    pub limit_kb: usize,
    /// Working directory for relative path display.
    pub cwd: Option<PathBuf>,
    /// Set when user confirms; consumed by main.rs to trigger send.
    pub outcome: Option<FileInjectionOutcome>,
}

impl FileInjectionDialogState {
    pub fn new() -> Self {
        Self {
            visible: false,
            pending_input: None,
            pending_imgs: Vec::new(),
            oversized: Vec::new(),
            selected: 0,
            limit_kb: 0,
            cwd: None,
            outcome: None,
        }
    }

    /// Show the dialog with stashed input and oversized files.
    pub fn show(
        &mut self,
        input: String,
        imgs: Vec<PastedImage>,
        oversized: Vec<(String, usize, AtFileIssue)>,
        limit_kb: usize,
        cwd: Option<PathBuf>,
    ) {
        self.visible = true;
        self.pending_input = Some(input);
        self.pending_imgs = imgs;
        self.oversized = oversized;
        self.limit_kb = limit_kb;
        self.cwd = cwd;
        // Directory-only: default to Abort (Allow does nothing useful for dirs)
        self.selected = if self.is_directory_only() { 1 } else { 0 };
        self.outcome = None;
    }

    /// Check if all oversized items are directories.
    pub fn is_directory_only(&self) -> bool {
        !self.oversized.is_empty()
            && self
                .oversized
                .iter()
                .all(|(_, _, issue)| matches!(issue, AtFileIssue::IsDirectory))
    }

    /// Returns the currently-selected outcome option.
    pub fn current_outcome(&self) -> FileInjectionOutcome {
        if self.selected == 0 {
            FileInjectionOutcome::InjectAll
        } else {
            FileInjectionOutcome::Abort
        }
    }

    /// Returns `true` if the currently-selected option is Allow.
    pub fn is_accept_selected(&self) -> bool {
        self.current_outcome() == FileInjectionOutcome::InjectAll
    }

    /// Confirm the selected option.
    pub fn confirm(&mut self) {
        self.outcome = Some(self.current_outcome());
    }

    /// Dismiss the dialog (Abort path).
    pub fn dismiss(&mut self) {
        self.visible = false;
        self.pending_input = None;
        self.pending_imgs.clear();
        self.oversized.clear();
        self.outcome = None;
    }

    /// Take the outcome (if set) along with stashed input and images.
    /// Returns None if no outcome is set.
    pub fn take_outcome(&mut self) -> Option<(FileInjectionOutcome, String, Vec<PastedImage>)> {
        let outcome = self.outcome.take()?;
        let input = self.pending_input.take()?;
        let imgs = std::mem::take(&mut self.pending_imgs);
        self.visible = false;
        Some((outcome, input, imgs))
    }

    /// Return a display path: relative to cwd when possible, otherwise absolute.
    pub fn display_path<'a>(&self, abs_path: &'a str) -> &'a str {
        if let Some(cwd) = &self.cwd {
            let cwd_str = cwd.to_string_lossy();
            // Strip cwd prefix plus trailing slash
            let prefix = format!("{}/", cwd_str);
            if let Some(rel) = abs_path.strip_prefix(prefix.as_str()) {
                return rel;
            }
            if abs_path == cwd_str.as_ref() {
                return ".";
            }
        }
        abs_path
    }
}

impl Default for FileInjectionDialogState {
    fn default() -> Self {
        Self::new()
    }
}

/// Render the file injection warning dialog over the frame.
pub fn render_file_injection_dialog(
    frame: &mut Frame,
    state: &FileInjectionDialogState,
    area: Rect,
) {
    if !state.visible {
        return;
    }

    let is_directory = state.is_directory_only();
    let n_files = state.oversized.len();

    // Height: 1 blank + 1 label + 1 blank + N files + 1 blank + 1 hint + 1 blank
    let content_rows = 5 + n_files;
    let dialog_height = (content_rows as u16 + 2).min(area.height.saturating_sub(4));
    let dialog_width = 72u16.min(area.width.saturating_sub(4));
    let dialog_area = Rect {
        x: (area.width.saturating_sub(dialog_width)) / 2,
        y: (area.height.saturating_sub(dialog_height).saturating_sub(3)) / 2,
        width: dialog_width,
        height: dialog_height,
    };

    let title = if is_directory {
        " ⚠  Directory Injection Warning "
    } else {
        " ⚠  File Injection Warning "
    };

    let bg = Color::Rgb(35, 35, 35);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(vec![Span::styled(
            title,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]))
        .border_style(Style::default().fg(Color::Yellow))
        .style(Style::default().bg(bg))
        .padding(Padding {
            left: 2,
            right: 2,
            top: 0,
            bottom: 0,
        });

    let inner = block.inner(dialog_area);
    frame.render_widget(Clear, dialog_area);
    frame.render_widget(block, dialog_area);

    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(Line::from(""));

    // Header: label + limit info
    let label_word = if is_directory {
        if n_files == 1 {
            "directory"
        } else {
            "directories"
        }
    } else if n_files == 1 {
        "file"
    } else {
        "files"
    };
    let header = if is_directory {
        format!("The following {} cannot be auto-injected:", label_word)
    } else if state.limit_kb > 0 {
        format!(
            "The following {} is over the file size limit ({} KB):",
            label_word, state.limit_kb
        )
    } else {
        format!("The following {} cannot be auto-injected:", label_word)
    };

    lines.push(Line::from(vec![Span::styled(
        header,
        Style::default().fg(Color::White),
    )]));
    lines.push(Line::from(""));

    for (path, _size_kb, issue) in &state.oversized {
        let display = state.display_path(path).to_owned();
        let text = match issue {
            AtFileIssue::Binary => format!("• {} (binary)", display),
            AtFileIssue::TooLarge(_) => format!("• {} (too large)", display),
            AtFileIssue::Unreadable(msg) => format!("• {} (unreadable: {})", display, msg),
            AtFileIssue::IsDirectory => format!("• {}", display),
        };

        lines.push(Line::from(vec![Span::styled(
            text,
            Style::default().fg(Color::DarkGray),
        )]));
    }

    lines.push(Line::from(""));

    if is_directory {
        lines.push(Line::from(vec![Span::styled(
            "  Enter or Esc to dismiss",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )]));
    } else {
        lines.push(Line::from(vec![
            Span::styled(
                "  Enter to inject anyway",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  ·  Esc to abort",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
        ]));
    }

    lines.push(Line::from(""));

    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(inner, frame.buffer_mut());
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn file_injection_dialog_defaults_hidden() {
        let state = FileInjectionDialogState::new();
        assert!(!state.visible);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn file_injection_dialog_show_sets_visible() {
        let mut state = FileInjectionDialogState::new();
        state.show(
            "input".to_string(),
            vec![],
            vec![("file.txt".to_string(), 100, AtFileIssue::TooLarge(100))],
            100,
            None,
        );
        assert!(state.visible);
        assert_eq!(state.selected, 0);
        assert!(!state.oversized.is_empty());
    }

    #[test]
    fn file_injection_dialog_directory_only_defaults_to_abort() {
        let mut state = FileInjectionDialogState::new();
        state.show(
            "input".to_string(),
            vec![],
            vec![("dir".to_string(), 0, AtFileIssue::IsDirectory)],
            0,
            None,
        );
        assert_eq!(state.current_outcome(), FileInjectionOutcome::Abort);
    }

    #[test]
    fn file_injection_dialog_confirm_allow() {
        let mut state = FileInjectionDialogState::new();
        state.show("input".to_string(), vec![], vec![], 100, None);
        state.confirm();
        assert_eq!(state.outcome, Some(FileInjectionOutcome::InjectAll));
    }

    #[test]
    fn file_injection_dialog_confirm_abort() {
        let mut state = FileInjectionDialogState::new();
        state.show("input".to_string(), vec![], vec![], 100, None);
        state.selected = 1; // Abort
        state.confirm();
        assert_eq!(state.outcome, Some(FileInjectionOutcome::Abort));
    }

    #[test]
    fn file_injection_dialog_take_outcome() {
        let mut state = FileInjectionDialogState::new();
        state.show("test input".to_string(), vec![], vec![], 100, None);
        state.confirm(); // Allow
        let (outcome, input, _) = state.take_outcome().unwrap();
        assert_eq!(outcome, FileInjectionOutcome::InjectAll);
        assert_eq!(input, "test input");
        assert!(!state.visible);
        assert_eq!(state.outcome, None);
    }

    #[test]
    fn file_injection_dialog_renders_without_panic() {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut state = FileInjectionDialogState::new();
        state.show(
            "input".to_string(),
            vec![],
            vec![("large_file.rs".to_string(), 250, AtFileIssue::TooLarge(250))],
            100,
            None,
        );
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_file_injection_dialog(frame, &state, area);
            })
            .unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .clone()
            .content()
            .iter()
            .map(|c| c.symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(content.contains("Warning") || content.contains("File"));
    }

    #[test]
    fn file_injection_dialog_hidden_renders_nothing() {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let state = FileInjectionDialogState::new(); // visible = false
        let before = terminal.backend().buffer().clone();
        terminal
            .draw(|frame| {
                render_file_injection_dialog(frame, &state, frame.area());
            })
            .unwrap();
        assert_eq!(terminal.backend().buffer().content(), before.content());
    }

    #[test]
    fn display_path_strips_cwd() {
        let mut state = FileInjectionDialogState::new();
        state.cwd = Some(PathBuf::from("/home/user/project"));
        assert_eq!(
            state.display_path("/home/user/project/src/main.rs"),
            "src/main.rs"
        );
        assert_eq!(state.display_path("/other/path"), "/other/path");
    }

    #[test]
    fn display_path_without_cwd_returns_input_unchanged() {
        let state = FileInjectionDialogState::new(); // cwd = None
        assert_eq!(
            state.display_path("/some/absolute/path.rs"),
            "/some/absolute/path.rs"
        );
    }

    fn render_to_string(state: &FileInjectionDialogState) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| {
                render_file_injection_dialog(frame, state, frame.area());
            })
            .unwrap();
        terminal
            .backend()
            .buffer()
            .clone()
            .content()
            .iter()
            .map(|c| c.symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    #[test]
    fn file_injection_dialog_renders_too_large_annotation() {
        let mut state = FileInjectionDialogState::new();
        state.show(
            "input".to_string(),
            vec![],
            vec![("large.rs".to_string(), 250, AtFileIssue::TooLarge(250))],
            100,
            None,
        );
        let content = render_to_string(&state);
        assert!(
            content.contains("too large"),
            "Expected '(too large)' annotation in rendered output"
        );
    }

    #[test]
    fn file_injection_dialog_renders_unreadable_annotation() {
        let mut state = FileInjectionDialogState::new();
        state.show(
            "input".to_string(),
            vec![],
            vec![(
                "secret.rs".to_string(),
                0,
                AtFileIssue::Unreadable("Permission denied".to_string()),
            )],
            0,
            None,
        );
        let content = render_to_string(&state);
        assert!(
            content.contains("unreadable"),
            "Expected '(unreadable: ...)' annotation in rendered output"
        );
    }

    #[test]
    fn file_injection_dialog_hint_uses_anyway_not_anyways() {
        let mut state = FileInjectionDialogState::new();
        state.show(
            "input".to_string(),
            vec![],
            vec![("file.rs".to_string(), 250, AtFileIssue::TooLarge(250))],
            100,
            None,
        );
        let content = render_to_string(&state);
        assert!(
            !content.contains("anyways"),
            "Should use 'anyway' not 'anyways'"
        );
        assert!(content.contains("anyway"), "Expected 'anyway' in hint text");
    }
}
