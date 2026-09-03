//! Tests for the app module.

use super::*;

use claurst_core::config::Config;
use claurst_core::types::Role;
use super::keys::{
    key_event_to_keystroke, layout_to_latin, normalize_char_with_shift,
    normalize_layout_shortcut_key,
};
use super::types::{ContextMenuItem, ContextMenuState};

    
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    fn make_app() -> App {
        let config = Config::default();
        let cost_tracker = claurst_core::cost::CostTracker::new();
        App::new(config, cost_tracker)
    }

    fn press_key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    // ---- recent-activity label (issue #277) ----

    #[test]
    fn recent_session_label_prefers_title() {
        let label = recent_session_label(
            Some("My Title".to_string()),
            Some("some prompt".to_string()),
        );
        assert_eq!(label, "My Title");
    }

    #[test]
    fn recent_session_label_falls_back_to_first_prompt_line() {
        let label = recent_session_label(
            None,
            Some("  fix the bug\nand more details".to_string()),
        );
        assert_eq!(label, "fix the bug");
    }

    #[test]
    fn recent_session_label_skips_blank_title_and_untitled_default() {
        // Blank/whitespace title is ignored in favour of the prompt.
        assert_eq!(
            recent_session_label(Some("   ".to_string()), Some("do it".to_string())),
            "do it"
        );
        // Nothing usable → untitled.
        assert_eq!(recent_session_label(None, None), "(untitled)");
        assert_eq!(
            recent_session_label(Some(String::new()), Some("\n\n".to_string())),
            "(untitled)"
        );
    }

    #[test]
    fn recent_session_label_truncates_long_prompt() {
        let long = "x".repeat(200);
        let label = recent_session_label(None, Some(long));
        assert_eq!(label.chars().count(), 80);
    }

    // ---- mouse capture gate (issue #104) ----

    fn scroll_up_event() -> crossterm::event::MouseEvent {
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::ScrollUp,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn mouse_events_processed_when_capture_enabled() {
        // Default config leaves mouse capture on, so a scroll wheel event
        // should move the scroll offset — provided there is content to scroll
        // over (a render must have established a non-zero max_scroll).
        let mut app = make_app();
        assert!(app.config.mouse_capture_enabled());
        assert_eq!(app.scroll_offset, 0);
        app.last_max_scroll.set(50);
        app.handle_mouse_event(scroll_up_event());
        assert!(app.scroll_offset > 0, "scroll should advance when capture is on");
        assert!(app.scroll_offset <= 50, "scroll stays within max_scroll");
    }

    // ---- click-to-view paste placeholders ----

    #[test]
    fn prompt_click_on_placeholder_opens_viewer() {
        let mut app = make_app();
        // Bottom pane as rendered: 1 status row (height > 2), then the top
        // separator at y=21, text rows from y=22. Prefix "❯ " is 2 cells.
        app.last_input_area.set(ratatui::layout::Rect { x: 0, y: 20, width: 80, height: 8 });
        for c in "hi ".chars() {
            app.prompt_input.insert_char(c);
        }
        app.prompt_input.paste("l1\nl2\nl3");
        assert!(app.prompt_input.text.contains("[Pasted text #1"));

        // Click on the separator row: nothing opens.
        app.handle_prompt_click(10, 21);
        assert!(!app.paste_viewer.visible);

        // Click inside the placeholder on the first text row: the viewer
        // opens read-only — the placeholder stays in the buffer and the body
        // stays stored so submit-time expansion is unaffected.
        app.handle_prompt_click(2 + 5, 22);
        assert!(app.paste_viewer.visible);
        assert_eq!(app.paste_viewer.paste_id, 1);
        assert_eq!(app.paste_viewer.line_count(), 3);
        assert!(app.prompt_input.text.contains("[Pasted text #1"));
        assert!(!app.prompt_input.paste_contents.is_empty());
    }

    #[test]
    fn paste_viewer_alt_e_expands_into_prompt() {
        let mut app = make_app();
        app.last_input_area.set(ratatui::layout::Rect { x: 0, y: 20, width: 80, height: 8 });
        for c in "hi ".chars() {
            app.prompt_input.insert_char(c);
        }
        app.prompt_input.paste("l1\nl2\nl3");
        app.handle_prompt_click(2 + 5, 22);
        assert!(app.paste_viewer.visible);

        let alt_e = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('e'),
            KeyModifiers::ALT,
        );
        app.handle_paste_viewer_key(alt_e);
        assert!(!app.paste_viewer.visible);
        assert_eq!(app.prompt_input.text, "hi l1\nl2\nl3");
        assert!(app.prompt_input.paste_contents.is_empty());
    }

    #[test]
    fn prompt_click_off_placeholder_moves_cursor_only() {
        let mut app = make_app();
        app.last_input_area.set(ratatui::layout::Rect { x: 0, y: 20, width: 80, height: 8 });
        for c in "hello ".chars() {
            app.prompt_input.insert_char(c);
        }
        app.prompt_input.paste("l1\nl2\nl3");
        let text_before = app.prompt_input.text.clone();

        // Click on "hello " before the placeholder: cursor moves, no viewer.
        app.handle_prompt_click(2 + 1, 22);
        assert_eq!(app.prompt_input.text, text_before);
        assert_eq!(app.prompt_input.cursor, 1);
        assert!(!app.paste_viewer.visible);
    }

    // ---- scroll_offset clamping (issue #223) ----

    #[test]
    fn scroll_up_offset_clamped_to_max_scroll() {
        let mut app = make_app();
        // A render established that the transcript is 5 lines taller than the
        // viewport, so scroll_offset can meaningfully range over 0..=5.
        app.last_max_scroll.set(5);

        // Scroll up far past the top, many times.
        for _ in 0..50 {
            app.scroll_up_by(10);
        }

        // Without the clamp scroll_offset would be 500; it must stay at
        // max_scroll so the offset can't inflate unboundedly (#223).
        assert_eq!(
            app.scroll_offset, 5,
            "scroll_offset must not inflate past max_scroll"
        );
        assert!(!app.auto_scroll, "scrolling up disables auto-follow");

        // Because it was clamped, a single Down step moves the view
        // immediately instead of burning through hundreds of wasted presses.
        let before = app.scroll_offset;
        app.scroll_offset = app.scroll_offset.saturating_sub(1);
        assert!(
            app.scroll_offset < before,
            "a single Down moves the view once scroll_offset is clamped"
        );
    }

    #[test]
    fn scroll_up_no_op_when_nothing_to_scroll() {
        // When content fits the viewport (max_scroll == 0) scrolling up is a
        // no-op rather than silently inflating scroll_offset.
        let mut app = make_app();
        app.last_max_scroll.set(0);
        for _ in 0..20 {
            app.scroll_up_by(10);
        }
        assert_eq!(app.scroll_offset, 0, "no scroll room means no offset growth");
    }

    #[test]
    fn mouse_events_ignored_when_capture_disabled() {
        // With mouseCapture: false the app must not act on mouse events that
        // still slip through, so the scroll offset stays put.
        let mut app = make_app();
        app.config.mouse_capture = Some(false);
        assert!(!app.config.mouse_capture_enabled());
        app.handle_mouse_event(scroll_up_event());
        assert_eq!(app.scroll_offset, 0, "scroll must not move when capture is off");
    }

    // ---- normalize_char_with_shift tests ----

    #[test]
    fn test_normalize_char_no_shift_returns_unchanged() {
        assert_eq!(normalize_char_with_shift('a', KeyModifiers::NONE), 'a');
        assert_eq!(normalize_char_with_shift('1', KeyModifiers::NONE), '1');
        assert_eq!(normalize_char_with_shift('!', KeyModifiers::NONE), '!');
    }

    #[test]
    fn test_normalize_char_shift_uppercase_letters() {
        assert_eq!(normalize_char_with_shift('a', KeyModifiers::SHIFT), 'A');
        assert_eq!(normalize_char_with_shift('z', KeyModifiers::SHIFT), 'Z');
        assert_eq!(normalize_char_with_shift('m', KeyModifiers::SHIFT), 'M');
    }

    #[test]
    fn test_normalize_char_shift_numbers() {
        assert_eq!(normalize_char_with_shift('1', KeyModifiers::SHIFT), '!');
        assert_eq!(normalize_char_with_shift('2', KeyModifiers::SHIFT), '@');
        assert_eq!(normalize_char_with_shift('3', KeyModifiers::SHIFT), '#');
        assert_eq!(normalize_char_with_shift('4', KeyModifiers::SHIFT), '$');
        assert_eq!(normalize_char_with_shift('5', KeyModifiers::SHIFT), '%');
        assert_eq!(normalize_char_with_shift('6', KeyModifiers::SHIFT), '^');
        assert_eq!(normalize_char_with_shift('7', KeyModifiers::SHIFT), '&');
        assert_eq!(normalize_char_with_shift('8', KeyModifiers::SHIFT), '*');
        assert_eq!(normalize_char_with_shift('9', KeyModifiers::SHIFT), '(');
        assert_eq!(normalize_char_with_shift('0', KeyModifiers::SHIFT), ')');
    }

    #[test]
    fn test_normalize_char_shift_symbols() {
        assert_eq!(normalize_char_with_shift('-', KeyModifiers::SHIFT), '_');
        assert_eq!(normalize_char_with_shift('=', KeyModifiers::SHIFT), '+');
        assert_eq!(normalize_char_with_shift('[', KeyModifiers::SHIFT), '{');
        assert_eq!(normalize_char_with_shift(']', KeyModifiers::SHIFT), '}');
        assert_eq!(normalize_char_with_shift(';', KeyModifiers::SHIFT), ':');
        assert_eq!(normalize_char_with_shift('\'', KeyModifiers::SHIFT), '"');
        assert_eq!(normalize_char_with_shift(',', KeyModifiers::SHIFT), '<');
        assert_eq!(normalize_char_with_shift('.', KeyModifiers::SHIFT), '>');
        assert_eq!(normalize_char_with_shift('/', KeyModifiers::SHIFT), '?');
        assert_eq!(normalize_char_with_shift('\\', KeyModifiers::SHIFT), '|');
        assert_eq!(normalize_char_with_shift('`', KeyModifiers::SHIFT), '~');
    }

    #[test]
    fn test_normalize_char_shift_already_shifted_chars_unchanged() {
        // Characters that don't have shift equivalents remain unchanged
        assert_eq!(normalize_char_with_shift('!', KeyModifiers::SHIFT), '!');
        assert_eq!(normalize_char_with_shift('@', KeyModifiers::SHIFT), '@');
        assert_eq!(normalize_char_with_shift('A', KeyModifiers::SHIFT), 'A');
    }

    #[test]
    fn test_normalize_char_other_modifiers_ignored() {
        // CTRL or ALT without SHIFT should not shift the character
        assert_eq!(normalize_char_with_shift('a', KeyModifiers::CONTROL), 'a');
        assert_eq!(normalize_char_with_shift('1', KeyModifiers::ALT), '1');
        assert_eq!(normalize_char_with_shift('a', KeyModifiers::CONTROL | KeyModifiers::ALT), 'a');
    }

    #[test]
    fn test_normalize_char_shift_with_other_modifiers() {
        // SHIFT + CTRL should still apply shift transformation
        assert_eq!(
            normalize_char_with_shift('a', KeyModifiers::SHIFT | KeyModifiers::CONTROL),
            'A'
        );
        assert_eq!(
            normalize_char_with_shift('1', KeyModifiers::SHIFT | KeyModifiers::ALT),
            '!'
        );
    }

    // ---- issue #183: slash command input & execution on Windows / non-kitty terminals ----

    #[test]
    fn test_slash_inserts_literal_slash_when_shift_flagged_on_non_kitty_terminal() {
        // On terminals that don't speak the kitty protocol (Windows conhost / CMD
        // / legacy PowerShell, and non-US layouts where `/` is a shifted key) the
        // slash key can arrive as Char('/') carrying a SHIFT flag, with the
        // character already final. We must insert a literal `/`, not re-shift it
        // into `?` (issue #183).
        let mut app = make_app();
        app.kitty_keyboard_active = false;
        // Pre-fill so the empty-prompt `?`/`/` help shortcut is out of the picture.
        app.prompt_input.text = "x".to_string();
        app.prompt_input.cursor = app.prompt_input.text.len();
        app.refresh_prompt_input();

        app.handle_key_event(press_key(KeyCode::Char('/'), KeyModifiers::SHIFT));

        assert_eq!(app.prompt_input.text, "x/");
    }

    #[test]
    fn test_slash_with_shift_flag_starts_command_not_help_on_non_kitty_terminal() {
        // Empty prompt: pressing `/` (reported as Char('/') + SHIFT on a non-kitty
        // terminal) must insert a literal slash so the user can start a command,
        // NOT toggle the help overlay (issue #183 — "Cannot run any slash commands").
        let mut app = make_app();
        app.kitty_keyboard_active = false;

        app.handle_key_event(press_key(KeyCode::Char('/'), KeyModifiers::SHIFT));

        assert!(
            !app.help_overlay.visible,
            "a literal slash must not open the help overlay"
        );
        assert!(!app.show_help);
        assert_eq!(app.prompt_input.text, "/");
    }

    #[test]
    fn test_shift_slash_still_normalizes_to_question_under_kitty_protocol() {
        // With the kitty protocol active, Shift+/ arrives as the unshifted base
        // key Char('/') + SHIFT, so we DO apply the US-QWERTY shift map → `?`.
        let mut app = make_app();
        app.kitty_keyboard_active = true;
        app.prompt_input.text = "x".to_string();
        app.prompt_input.cursor = app.prompt_input.text.len();
        app.refresh_prompt_input();

        app.handle_key_event(press_key(KeyCode::Char('/'), KeyModifiers::SHIFT));

        assert_eq!(app.prompt_input.text, "x?");
    }

    #[test]
    fn test_enter_runs_highlighted_slash_command_in_one_press() {
        // Typing a slash command and pressing Enter should run it immediately
        // rather than merely completing the text and waiting for a second Enter
        // (issue #183 — "enter will not run the command").
        let mut app = make_app();
        for c in "/help".chars() {
            app.handle_key_event(press_key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert!(
            !app.prompt_input.suggestions.is_empty(),
            "the slash-command popup should be open"
        );

        let should_submit = app.handle_key_event(press_key(KeyCode::Enter, KeyModifiers::NONE));

        assert!(should_submit, "Enter should submit/run the highlighted command");
        assert_eq!(app.prompt_input.text, "/help");
        assert!(
            app.prompt_input.suggestions.is_empty(),
            "the popup should be dismissed after running"
        );
    }

    #[test]
    fn test_enter_completes_slash_prefix_then_runs() {
        // Even from a unique prefix, Enter completes to the highlighted command
        // and runs it in a single press.
        let mut app = make_app();
        for c in "/the".chars() {
            app.handle_key_event(press_key(KeyCode::Char(c), KeyModifiers::NONE));
        }

        let should_submit = app.handle_key_event(press_key(KeyCode::Enter, KeyModifiers::NONE));

        assert!(should_submit);
        assert_eq!(app.prompt_input.text, "/theme");
    }

    // ---- Shift+Enter newline vs Enter submit (issue #224) ----

    /// Feed some text then a modified Enter and return (submitted?, buffer).
    fn type_then_modified_enter(mods: KeyModifiers) -> (bool, String) {
        let mut app = make_app();
        for c in "hi".chars() {
            app.handle_key_event(press_key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let submitted = app.handle_key_event(press_key(KeyCode::Enter, mods));
        (submitted, app.prompt_input.text.clone())
    }

    #[test]
    fn shift_enter_inserts_newline_not_submit() {
        // On kitty-capable terminals Shift+Enter arrives as Enter+SHIFT and must
        // insert a literal newline, leaving the prompt multi-line and unsent.
        let (submitted, text) = type_then_modified_enter(KeyModifiers::SHIFT);
        assert!(!submitted, "Shift+Enter must not submit");
        assert_eq!(text, "hi\n", "Shift+Enter should append a newline");
        assert!(text.contains('\n'), "buffer should now be multi-line");
    }

    #[test]
    fn alt_enter_inserts_newline_fallback() {
        // Alt+Enter is a fallback for terminals that can't report Shift+Enter.
        let (submitted, text) = type_then_modified_enter(KeyModifiers::ALT);
        assert!(!submitted, "Alt+Enter must not submit");
        assert_eq!(text, "hi\n");
    }

    #[test]
    fn ctrl_enter_inserts_newline_fallback() {
        // Ctrl+Enter is the Windows-Terminal-style fallback for newline.
        let (submitted, text) = type_then_modified_enter(KeyModifiers::CONTROL);
        assert!(!submitted, "Ctrl+Enter must not submit");
        assert_eq!(text, "hi\n");
    }

    #[test]
    fn ctrl_j_inserts_newline_fallback() {
        // Ctrl+J (Char('j') + CONTROL) is the conventional legacy newline escape
        // (pi binds insert-newline to shift+enter + ctrl+j). It must insert a
        // newline, not the literal character 'j'.
        let mut app = make_app();
        for c in "hi".chars() {
            app.handle_key_event(press_key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let submitted = app.handle_key_event(press_key(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert!(!submitted, "Ctrl+J must not submit");
        assert_eq!(app.prompt_input.text, "hi\n", "Ctrl+J should insert a newline, not 'j'");
    }

    #[test]
    fn bare_enter_submits_without_newline() {
        // A plain Enter (no modifiers) submits and leaves the buffer untouched.
        let mut app = make_app();
        for c in "hi".chars() {
            app.handle_key_event(press_key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let submitted = app.handle_key_event(press_key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(submitted, "bare Enter should submit");
        assert_eq!(app.prompt_input.text, "hi", "bare Enter must not insert a newline");
        assert!(!app.prompt_input.text.contains('\n'));
    }

    #[test]
    fn shift_enter_newline_composes_multiline_prompt() {
        // Compose two lines with Shift+Enter between them, then submit with a
        // bare Enter; the buffer keeps both lines and only the bare Enter sends.
        let mut app = make_app();
        for c in "line1".chars() {
            app.handle_key_event(press_key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert!(!app.handle_key_event(press_key(KeyCode::Enter, KeyModifiers::SHIFT)));
        for c in "line2".chars() {
            app.handle_key_event(press_key(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert_eq!(app.prompt_input.text, "line1\nline2");
        assert!(app.handle_key_event(press_key(KeyCode::Enter, KeyModifiers::NONE)));
    }

    #[test]
    fn test_mcp_subcommand_is_not_intercepted() {
        let mut app = make_app();
        assert!(!app.intercept_slash_command_with_args("mcp", "auth mcphub"));
        assert!(!app.mcp_view.visible);
    }

    #[test]
    fn test_clear_slash_command_clears_messages() {
        let mut app = make_app();
        app.add_message(Role::User, "hello".to_string());
        app.add_message(Role::Assistant, "world".to_string());
        assert_eq!(app.messages.len(), 2);
        assert!(app.intercept_slash_command("clear"));
        assert_eq!(app.messages.len(), 0);
    }

    #[test]
    fn test_exit_slash_command_sets_quit_flag() {
        let mut app = make_app();
        assert!(!app.should_exit);
        assert!(app.intercept_slash_command("exit"));
        assert!(app.should_exit);
    }

    #[test]
    fn test_vim_slash_command_toggles_vim() {
        let mut app = make_app();
        assert!(!app.prompt_input.vim_enabled);
        assert!(app.intercept_slash_command("vim"));
        assert!(app.prompt_input.vim_enabled);
        assert!(app.intercept_slash_command("vim"));
        assert!(!app.prompt_input.vim_enabled);
    }

    #[test]
    fn test_model_slash_command_opens_picker() {
        let mut app = make_app();
        assert!(!app.model_picker.visible);
        assert!(app.intercept_slash_command("model"));
        assert!(app.model_picker.visible);
    }

    #[test]
    fn test_fast_slash_command_toggles_fast_mode() {
        let mut app = make_app();
        assert!(!app.fast_mode);
        assert!(app.intercept_slash_command("fast"));
        assert!(app.fast_mode);
        assert!(app.intercept_slash_command("fast"));
        assert!(!app.fast_mode);
    }

    #[test]
    fn test_output_style_cycles() {
        let mut app = make_app();
        assert_eq!(app.output_style, "auto");
        assert!(app.intercept_slash_command("output-style"));
        assert_eq!(app.output_style, "stream");
        assert!(app.intercept_slash_command("output-style"));
        assert_eq!(app.output_style, "verbose");
        assert!(app.intercept_slash_command("output-style"));
        assert_eq!(app.output_style, "auto");
    }

    #[test]
    fn test_context_menu_fork_targets_clicked_message() {
        let mut app = make_app();
        app.add_message(Role::User, "one".to_string());
        app.add_message(Role::Assistant, "two".to_string());
        app.add_message(Role::User, "three".to_string());

        app.handle_context_menu_action(
            ContextMenuItem::Fork,
            ContextMenuKind::Message { message_index: 1 },
        );

        assert_eq!(app.prompt_input.text, "/fork 2");
        assert_eq!(
            app.status_message.as_deref(),
            Some("Fork at message 2 - press Enter to confirm")
        );
    }

    #[test]
    fn test_right_click_targets_row_message_instead_of_last_message() {
        use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

        let mut app = make_app();
        app.last_msg_area.set(ratatui::layout::Rect {
            x: 0,
            y: 0,
            width: 80,
            height: 10,
        });
        app.message_row_map.borrow_mut().insert(3, 1);

        app.handle_mouse_event(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 12,
            row: 3,
            modifiers: KeyModifiers::empty(),
        });

        assert!(matches!(
            app.context_menu_state,
            Some(ContextMenuState {
                kind: ContextMenuKind::Message { message_index: 1 },
                ..
            })
        ));
    }

    // ---- Help overlay -------------------------------------------------------

    #[test]
    fn test_help_slash_command_opens_overlay() {
        let mut app = make_app();
        assert!(!app.help_overlay.visible);
        assert!(!app.show_help);
        assert!(!app.help_overlay.commands.is_empty());
        assert!(app.intercept_slash_command("help"));
        assert!(app.help_overlay.visible);
        assert!(app.show_help);
    }

    #[test]
    fn test_help_slash_command_is_idempotent_when_already_open() {
        let mut app = make_app();
        // First call opens it.
        assert!(app.intercept_slash_command("help"));
        assert!(app.help_overlay.visible);
        // Second call while already open should leave it open (not toggle it off).
        assert!(app.intercept_slash_command("help"));
        assert!(app.help_overlay.visible);
    }

    #[test]
    fn test_question_mark_shortcut_opens_help_with_shift_modifier() {
        let mut app = make_app();

        app.handle_key_event(press_key(KeyCode::Char('?'), KeyModifiers::SHIFT));

        assert!(app.help_overlay.visible);
        assert!(app.show_help);
    }

    #[test]
    fn test_question_mark_shortcut_closes_help_with_shift_modifier() {
        let mut app = make_app();
        app.help_overlay.toggle();
        app.show_help = true;

        app.handle_key_event(press_key(KeyCode::Char('?'), KeyModifiers::SHIFT));

        assert!(!app.help_overlay.visible);
        assert!(!app.show_help);
    }

    #[test]
    fn test_question_mark_shortcut_types_into_non_empty_prompt() {
        let mut app = make_app();
        app.prompt_input.text = "why".to_string();
        app.prompt_input.cursor = app.prompt_input.text.len();
        app.refresh_prompt_input();

        app.handle_key_event(press_key(KeyCode::Char('?'), KeyModifiers::SHIFT));

        assert!(!app.help_overlay.visible);
        assert_eq!(app.prompt_input.text, "why?");
    }

    #[test]
    fn test_ctrl_shift_a_shortcut_opens_model_picker() {
        let mut app = make_app();
        app.has_credentials = true;
        app.config.provider = Some("anthropic".to_string());

        // The model-picker shortcut moved from Ctrl+A to Ctrl+Shift+A in
        // commit 8da4a29 to resolve the Ctrl+A conflict (goLineStart in the
        // prompt). The default bindings map ctrl+shift+a -> openModelPicker.
        app.handle_key_event(press_key(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));

        assert!(app.model_picker.visible);
    }

    #[test]
    fn test_ctrl_k_shortcut_opens_command_palette_even_with_input() {
        let mut app = make_app();
        app.prompt_input.text = "hello".to_string();
        app.prompt_input.cursor = app.prompt_input.text.len();
        app.refresh_prompt_input();

        app.handle_key_event(press_key(KeyCode::Char('k'), KeyModifiers::CONTROL));

        assert!(app.command_palette.visible);
        assert_eq!(app.prompt_input.text, "hello");
    }

    // ---- Bash prefix allowlist ----------------------------------------------

    #[test]
    fn test_bash_command_not_allowed_by_default() {
        let app = make_app();
        assert!(!app.bash_command_allowed_by_prefix("git status"));
        assert!(!app.bash_command_allowed_by_prefix("ls -la"));
        assert!(!app.bash_command_allowed_by_prefix(""));
    }

    #[test]
    fn test_bash_prefix_allowlist_after_p_key() {
        use crate::dialogs::PermissionRequest;
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

        let mut app = make_app();
        // Set up a bash permission dialog with a suggested prefix.
        let pr = PermissionRequest::bash(
            "tu-1".to_string(),
            "Bash".to_string(),
            "This will execute a shell command.".to_string(),
            "git status".to_string(),
            Some("git".to_string()),
        );
        app.permission_request = Some(pr);

        // Simulate pressing 'P' (prefix-allow key).
        let key = KeyEvent {
            code: KeyCode::Char('P'),
            modifiers: KeyModifiers::SHIFT,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_permission_key(key);

        // Dialog should be dismissed and "git" added to the allowlist.
        assert!(app.permission_request.is_none());
        assert!(app.bash_command_allowed_by_prefix("git status"));
        assert!(app.bash_command_allowed_by_prefix("git push origin main"));
        // Other commands should NOT be allowed.
        assert!(!app.bash_command_allowed_by_prefix("rm -rf /tmp"));
    }

    #[test]
    fn test_bash_prefix_allowlist_via_enter_on_p_option() {
        use crate::dialogs::PermissionRequest;
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

        let mut app = make_app();
        let mut pr = PermissionRequest::bash(
            "tu-2".to_string(),
            "Bash".to_string(),
            "This will execute a shell command.".to_string(),
            "cargo build".to_string(),
            Some("cargo".to_string()),
        );
        // Navigate to the prefix option (index 3 in a 5-option dialog).
        pr.selected_option = 3;
        app.permission_request = Some(pr);

        // Press Enter to confirm the currently selected (prefix) option.
        let key = KeyEvent {
            code: KeyCode::Enter,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_permission_key(key);

        assert!(app.permission_request.is_none());
        assert!(app.bash_command_allowed_by_prefix("cargo test"));
        assert!(!app.bash_command_allowed_by_prefix("make build"));
    }

    #[test]
    fn test_bash_prefix_allowlist_non_prefix_option_does_not_add() {
        use crate::dialogs::PermissionRequest;
        use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

        let mut app = make_app();
        let pr = PermissionRequest::bash(
            "tu-3".to_string(),
            "Bash".to_string(),
            "This will execute a shell command.".to_string(),
            "npm install".to_string(),
            Some("npm".to_string()),
        );
        app.permission_request = Some(pr);

        // Press 'y' (allow-once) — should NOT add to allowlist.
        let key = KeyEvent {
            code: KeyCode::Char('y'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        };
        app.handle_permission_key(key);

        assert!(app.permission_request.is_none());
        assert!(!app.bash_command_allowed_by_prefix("npm test"));
    }

    // ---- issue #47: shortcuts on non-English (Cyrillic) keyboard layouts ----

    #[test]
    fn test_layout_to_latin_maps_cyrillic_shortcut_positions() {
        // Letters used by core Ctrl/Alt shortcuts must resolve to the Latin key
        // at the same physical QWERTY position on the Russian/Ukrainian JCUKEN
        // layout. (left = Cyrillic glyph reported by the terminal, right = Latin)
        assert_eq!(layout_to_latin('с'), "c"); // Ctrl+C  (interrupt / exit)
        assert_eq!(layout_to_latin('в'), "d"); // Ctrl+D  (exit)
        assert_eq!(layout_to_latin('к'), "r"); // Ctrl+R  (history search)
        assert_eq!(layout_to_latin('и'), "b"); // Ctrl+B  (create branch)
        assert_eq!(layout_to_latin('з'), "p"); // Ctrl+P  (global search)
        assert_eq!(layout_to_latin('е'), "t"); // Ctrl+T  (tasks overlay)
        assert_eq!(layout_to_latin('т'), "n"); // n
        assert_eq!(layout_to_latin('о'), "j"); // Ctrl+J  (newline fallback)
        assert_eq!(layout_to_latin('г'), "u"); // Ctrl+U  (kill to start)
        assert_eq!(layout_to_latin('ц'), "w"); // Ctrl+W  (kill word)
        assert_eq!(layout_to_latin('л'), "k"); // Ctrl+K  (command palette)
        assert_eq!(layout_to_latin('а'), "f"); // Alt+F   (word forward)
        assert_eq!(layout_to_latin('н'), "y"); // Ctrl+Y  (yank)
    }

    #[test]
    fn test_layout_to_latin_covers_full_qwerty_letter_row() {
        // Every Latin letter position should be reachable from some Cyrillic key,
        // so every Ctrl/Alt+<letter> binding works regardless of layout.
        let cyrillic = "йцукенгшщзфывапролдячсмить";
        let mut latin: Vec<char> = cyrillic
            .chars()
            .filter_map(|c| layout_to_latin(c).chars().next())
            .filter(|c| c.is_ascii_alphabetic())
            .collect();
        latin.sort_unstable();
        latin.dedup();
        assert_eq!(latin.len(), 26, "all 26 Latin letters must be covered");
    }

    #[test]
    fn test_layout_to_latin_uppercase_cyrillic_folds_to_lowercase_latin() {
        // Shift+Ctrl on a Cyrillic layout reports the uppercase glyph.
        assert_eq!(layout_to_latin('С'), "c");
        assert_eq!(layout_to_latin('В'), "d");
    }

    #[test]
    fn test_layout_to_latin_passes_through_unknown_chars() {
        // Plain ASCII and unmapped characters are returned unchanged (lowercased).
        assert_eq!(layout_to_latin('c'), "c");
        assert_eq!(layout_to_latin('A'), "a");
    }

    #[test]
    fn test_key_event_to_keystroke_maps_ctrl_cyrillic_to_latin() {
        // Ctrl+С (Cyrillic) on a non-Latin layout must resolve to the Latin "c".
        let ks = key_event_to_keystroke(&press_key(
            KeyCode::Char('с'),
            KeyModifiers::CONTROL,
        ))
        .expect("keystroke");
        assert_eq!(ks.key, "c");
        assert!(ks.ctrl);

        // Ctrl+О (Cyrillic, the physical J key) → "j" so Ctrl+J newline works.
        let ks = key_event_to_keystroke(&press_key(
            KeyCode::Char('о'),
            KeyModifiers::CONTROL,
        ))
        .expect("keystroke");
        assert_eq!(ks.key, "j");
    }

    #[test]
    fn test_key_event_to_keystroke_keeps_plain_cyrillic_for_text_entry() {
        // Without a modifier the character must NOT be Latinized — it is literal
        // text the user is typing.
        let ks = key_event_to_keystroke(&press_key(KeyCode::Char('с'), KeyModifiers::NONE))
            .expect("keystroke");
        assert_eq!(ks.key, "с");
        assert!(!ks.ctrl && !ks.alt);
    }

    #[test]
    fn test_normalize_layout_shortcut_key_rewrites_pure_ctrl() {
        // Pure Ctrl + Cyrillic → Latin letter at the same physical position.
        let out = normalize_layout_shortcut_key(press_key(
            KeyCode::Char('с'),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(out.code, KeyCode::Char('c'));
        assert!(out.modifiers.contains(KeyModifiers::CONTROL));
    }

    #[test]
    fn test_normalize_layout_shortcut_key_leaves_plain_and_altgr_untouched() {
        // No modifier: literal text entry — must stay Cyrillic.
        let out = normalize_layout_shortcut_key(press_key(KeyCode::Char('с'), KeyModifiers::NONE));
        assert_eq!(out.code, KeyCode::Char('с'));

        // Ctrl+Alt (AltGr) can compose characters on some layouts — leave it.
        let out = normalize_layout_shortcut_key(press_key(
            KeyCode::Char('с'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert_eq!(out.code, KeyCode::Char('с'));

        // Plain Alt is also left alone (avoid disturbing Option/meta composition).
        let out = normalize_layout_shortcut_key(press_key(KeyCode::Char('с'), KeyModifiers::ALT));
        assert_eq!(out.code, KeyCode::Char('с'));
    }

    #[test]
    fn test_normalize_layout_shortcut_key_passes_ascii_through() {
        // ASCII Ctrl combos (English layout) are unchanged — no regression.
        let out = normalize_layout_shortcut_key(press_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(out.code, KeyCode::Char('c'));
    }

    #[test]
    fn test_ctrl_cyrillic_o_inserts_newline_like_ctrl_j() {
        // On a Cyrillic layout the physical Ctrl+J key reports Ctrl+О; it must
        // still insert a newline so multi-line composing works (issue #47).
        let mut app = make_app();
        app.prompt_input.text = "ab".to_string();
        app.prompt_input.cursor = app.prompt_input.text.len();
        app.refresh_prompt_input();

        app.handle_key_event(press_key(KeyCode::Char('о'), KeyModifiers::CONTROL));

        assert_eq!(app.prompt_input.text, "ab\n");
    }

    #[test]
    fn test_ctrl_j_inserts_newline_on_english_layout() {
        // Regression guard: the English Ctrl+J path still inserts a newline.
        let mut app = make_app();
        app.prompt_input.text = "ab".to_string();
        app.prompt_input.cursor = app.prompt_input.text.len();
        app.refresh_prompt_input();

        app.handle_key_event(press_key(KeyCode::Char('j'), KeyModifiers::CONTROL));

        assert_eq!(app.prompt_input.text, "ab\n");
    }

    #[test]
    fn test_raw_newline_char_inserts_newline() {
        // A bare LF (0x0A) arriving as Char('\n') — e.g. Shift+Enter on a
        // terminal without the kitty protocol — must add a newline, not be
        // dropped.
        let mut app = make_app();
        app.prompt_input.text = "ab".to_string();
        app.prompt_input.cursor = app.prompt_input.text.len();
        app.refresh_prompt_input();

        app.handle_key_event(press_key(KeyCode::Char('\n'), KeyModifiers::NONE));

        assert_eq!(app.prompt_input.text, "ab\n");
    }

    #[test]
    fn test_ctrl_cyrillic_c_triggers_exit_confirmation_on_cyrillic_layout() {
        // Ctrl+С (Cyrillic) on an empty prompt must arm the two-press exit
        // confirmation exactly like the English Ctrl+C (issue #47 — "Ctrl combos
        // don't work").
        let mut app = make_app();
        assert!(app.prompt_input.is_empty());

        app.handle_key_event(press_key(KeyCode::Char('с'), KeyModifiers::CONTROL));
        assert!(
            app.last_exit_key_warning.is_some(),
            "first Ctrl+С should arm the exit confirmation"
        );
        assert!(!app.should_exit);

        // Second press within the timeout exits.
        app.handle_key_event(press_key(KeyCode::Char('с'), KeyModifiers::CONTROL));
        assert!(app.should_exit, "second Ctrl+С should exit");
    }

    #[test]
    fn test_ctrl_c_still_triggers_exit_confirmation_on_english_layout() {
        // Regression guard: the English Ctrl+C exit confirmation is unchanged.
        let mut app = make_app();
        app.handle_key_event(press_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.last_exit_key_warning.is_some());
        assert!(!app.should_exit);
        app.handle_key_event(press_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.should_exit);
    }

    // ---- Keyboard text selection (Transcript context) ----

    #[test]
    fn test_kb_selection_enters_with_v_on_empty_prompt() {
        let mut app = make_app();
        app.last_msg_area.set(ratatui::layout::Rect::new(0, 0, 80, 24));
        app.last_selectable_area
            .set(ratatui::layout::Rect::new(0, 0, 80, 24));
        // Press 'v' on empty prompt — should enter selection mode.
        app.handle_key_event(press_key(KeyCode::Char('v'), KeyModifiers::NONE));
        assert!(app.kb_select_mode, "v on empty prompt should enter selection mode");
        assert!(app.selection_anchor.is_some(), "anchor should be set on entry");
        assert!(app.selection_focus.is_some(), "focus should be set on entry");
    }

    #[test]
    fn test_kb_selection_does_not_enter_with_text_in_prompt() {
        let mut app = make_app();
        app.last_msg_area.set(ratatui::layout::Rect::new(0, 0, 80, 24));
        app.last_selectable_area
            .set(ratatui::layout::Rect::new(0, 0, 80, 24));
        // Type "verify" — the 'v' should be inserted as text, not enter selection.
        app.handle_key_event(press_key(KeyCode::Char('v'), KeyModifiers::NONE));
        // First 'v' enters selection mode because prompt is empty.
        // Now type another 'v' — it should be swallowed by selection mode guard.
        assert!(app.kb_select_mode);
        app.handle_key_event(press_key(KeyCode::Char('e'), KeyModifiers::NONE));
        // 'e' is swallowed in selection mode, prompt stays empty.
        assert!(app.prompt_input.is_empty(), "keys should be swallowed in selection mode");
    }

    #[test]
    fn test_kb_selection_cancel_with_escape() {
        let mut app = make_app();
        app.last_msg_area.set(ratatui::layout::Rect::new(0, 0, 80, 24));
        app.last_selectable_area
            .set(ratatui::layout::Rect::new(0, 0, 80, 24));
        // Enter selection mode.
        app.handle_key_event(press_key(KeyCode::Char('v'), KeyModifiers::NONE));
        assert!(app.kb_select_mode);
        // Cancel with Escape.
        app.handle_key_event(press_key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.kb_select_mode, "Esc should exit selection mode");
        assert!(app.selection_anchor.is_none(), "selection should be cleared on cancel");
    }

    #[test]
    fn test_kb_selection_context_returns_transcript() {
        let mut app = make_app();
        // Not in selection mode — should return Chat.
        assert_eq!(
            app.current_key_context(),
            claurst_core::keybindings::KeyContext::Chat
        );
        // Enter selection mode.
        app.kb_select_mode = true;
        assert_eq!(
            app.current_key_context(),
            claurst_core::keybindings::KeyContext::Transcript
        );
    }

    #[test]
    fn test_kb_selection_ctrl_d_not_swallowed() {
        let mut app = make_app();
        app.last_msg_area.set(ratatui::layout::Rect::new(0, 0, 80, 24));
        app.last_selectable_area
            .set(ratatui::layout::Rect::new(0, 0, 80, 24));
        // Enter selection mode.
        app.handle_key_event(press_key(KeyCode::Char('v'), KeyModifiers::NONE));
        assert!(app.kb_select_mode);
        // Press Ctrl+D — should not be swallowed, should trigger exit confirmation.
        app.handle_key_event(press_key(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(
            app.last_exit_key_warning.is_some() || app.should_exit,
            "Ctrl+D should trigger exit confirmation, not be swallowed"
        );
    }

    #[test]
    fn test_kb_selection_move_cursor_down_then_up() {
        let mut app = make_app();
        // Set up a 10-row selectable area starting at row 5.
        app.last_selectable_area
            .set(ratatui::layout::Rect::new(0, 5, 80, 10));
        app.last_max_scroll.set(0);
        // Enter selection mode — starts at bottom (row 14).
        app.handle_key_event(press_key(KeyCode::Char('v'), KeyModifiers::NONE));
        assert_eq!(app.kb_cursor_row, 14, "should start at bottom row");
        // Move up one row.
        app.handle_key_event(press_key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.kb_cursor_row, 13, "up should move cursor up one row");
        // Move down one row.
        app.handle_key_event(press_key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.kb_cursor_row, 14, "down should move cursor back down");
    }

    #[test]
    fn test_kb_selection_clamps_at_bounds() {
        let mut app = make_app();
        app.last_selectable_area
            .set(ratatui::layout::Rect::new(0, 5, 80, 10));
        app.last_max_scroll.set(0);
        // Enter at bottom (row 14).
        app.handle_key_event(press_key(KeyCode::Char('v'), KeyModifiers::NONE));
        assert_eq!(app.kb_cursor_row, 14);
        // Move down — should clamp at 14.
        app.handle_key_event(press_key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.kb_cursor_row, 14, "should clamp at bottom");
        // Go to top.
        app.handle_key_event(press_key(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(app.kb_cursor_row, 5, "Home should go to top row");
        // Move up — should clamp at 5.
        app.handle_key_event(press_key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.kb_cursor_row, 5, "should clamp at top");
    }

    #[test]
    fn test_kb_selection_copy_exits_mode() {
        let mut app = make_app();
        app.last_selectable_area
            .set(ratatui::layout::Rect::new(0, 5, 80, 10));
        app.last_max_scroll.set(0);
        // Enter selection mode.
        app.handle_key_event(press_key(KeyCode::Char('v'), KeyModifiers::NONE));
        assert!(app.kb_select_mode);
        // Move up to create a non-empty selection.
        app.handle_key_event(press_key(KeyCode::Up, KeyModifiers::NONE));
        // Press Enter — should copy and exit.
        app.handle_key_event(press_key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.kb_select_mode, "Enter should exit selection mode after copy");
        assert!(app.selection_anchor.is_none(), "selection should be cleared after copy");
    }

    // ---- Render pipeline integration: selectable area restriction ----

    #[test]
    fn test_render_restricts_selectable_area_to_transcript() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = make_app();
        // Add a message so the transcript is non-empty and the welcome
        // box is not shown.
        app.messages.push(Message::user("Hello, this is a test message."));
        app.messages.push(Message::assistant("And this is a response."));

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::render::render_app(f, &app)).unwrap();

        // After rendering with messages, last_selectable_area should be
        // the message pane (last_msg_area), NOT the full terminal.
        let selectable = app.last_selectable_area.get();
        let full = ratatui::layout::Rect::new(0, 0, 80, 24);
        assert!(
            selectable.height < full.height,
            "selectable area height ({}) should be less than terminal height ({}) — \
             it must not include the prompt box and status bar",
            selectable.height,
            full.height
        );
        assert!(
            selectable.y > 0 || selectable.height < full.height,
            "selectable area should be restricted to the transcript pane, not the full terminal"
        );
        // last_msg_area should also be set.
        let msg_area = app.last_msg_area.get();
        assert_eq!(
            selectable, msg_area,
            "selectable area should match last_msg_area after render with messages"
        );
        // total_message_lines should have been written (was always 0 before).
        assert!(
            app.total_message_lines.get() > 0,
            "total_message_lines should be > 0 after rendering messages, got {}",
            app.total_message_lines.get()
        );
    }

    #[test]
    fn test_render_selectable_area_falls_back_on_empty_transcript() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let app = make_app();
        // No messages — welcome screen shown.
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::render::render_app(f, &app)).unwrap();

        // With no messages, last_msg_area is empty (0x0), so
        // last_selectable_area should fall back to the full frame.
        let selectable = app.last_selectable_area.get();
        let full = ratatui::layout::Rect::new(0, 0, 80, 24);
        assert_eq!(
            selectable, full,
            "selectable area should be the full terminal on the welcome screen"
        );
    }

    #[test]
    fn test_kb_selection_full_pipeline_enter_move_copy() {
        // End-to-end test: create app with messages, render to populate
        // last_selectable_area and total_message_lines, then drive the
        // full key event pipeline (v -> up -> enter) through
        // handle_key_event, which exercises the real keybinding resolver.
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut app = make_app();
        app.messages.push(Message::user("First message."));
        app.messages.push(Message::assistant("Second message."));
        app.messages.push(Message::user("Third message."));

        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| crate::render::render_app(f, &app)).unwrap();

        // Verify render populated the area.
        assert!(app.last_selectable_area.get().height > 0);

        // Enter selection mode via the real key pipeline.
        app.handle_key_event(press_key(KeyCode::Char('v'), KeyModifiers::NONE));
        assert!(app.kb_select_mode, "should enter selection mode after render");

        // Move up — should go through the Transcript keybinding context
        // and call selectionUp -> move_kb_cursor(-1).
        let start_row = app.kb_cursor_row;
        app.handle_key_event(press_key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(
            app.kb_cursor_row,
            start_row.saturating_sub(1),
            "up should move cursor up one row after render pipeline"
        );

        // Move back down.
        app.handle_key_event(press_key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(app.kb_cursor_row, start_row, "down should restore cursor position");

        // Enter copies and exits (selection may be empty at anchor==focus
        // but the action handler still fires and exits cleanly).
        app.handle_key_event(press_key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.kb_select_mode, "Enter should exit selection mode");
    }
