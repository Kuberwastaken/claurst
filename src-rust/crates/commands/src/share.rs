// Session-sharing commands: `/share` and `/links`.
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct ShareCommand;
pub struct LinksCommand;

// ---- /share --------------------------------------------------------------

#[async_trait]
impl SlashCommand for ShareCommand {
    fn name(&self) -> &str { "share" }
    fn description(&self) -> &str {
        "Upload the current session as a secret GitHub gist and return a shareable URL"
    }
    fn help(&self) -> &str {
        "Usage: /share\n\n\
         Renders the current session as a single self-contained HTML file,\n\
         uploads it as a secret GitHub gist via the `gh` CLI, and prints a\n\
         viewer URL of the form https://claurst.kuber.studio/session/#<gist-id>.\n\n\
         Requirements:\n  \
           - GitHub CLI (gh) installed and logged in (`gh auth login`).\n\n\
         The viewer base URL can be overridden with CLAURST_SHARE_VIEWER_URL.\n\
         Secret gists are unlisted but readable by anyone who has the link."
    }

    async fn execute(&self, _args: &str, ctx: &mut CommandContext) -> CommandResult {
        use claurst_core::share_export::{share_viewer_url, write_session_html, SessionExportMeta};

        // 1. Check that `gh` is installed and authenticated. Uses tokio::process
        //    so the TUI event loop keeps animating during the (occasionally
        //    slow) network round-trip.
        match tokio::process::Command::new("gh")
            .args(["auth", "status"])
            .output()
            .await
        {
            Err(_) => {
                return CommandResult::Error(
                    "GitHub CLI (gh) is not installed. Install it from https://cli.github.com/"
                        .to_string(),
                );
            }
            Ok(out) if !out.status.success() => {
                return CommandResult::Error(
                    "GitHub CLI is not logged in. Run `gh auth login` first.".to_string(),
                );
            }
            Ok(_) => {}
        }

        // 2. Build metadata + render HTML to a temp file.
        let meta = SessionExportMeta {
            session_id: ctx.session_id.clone(),
            title: ctx.session_title.clone(),
            model: ctx.config.effective_model().to_string(),
            working_dir: ctx.working_dir.display().to_string(),
            exported_at: chrono::Utc::now().to_rfc3339(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
        };

        let safe_id: String = ctx
            .session_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        let stem = if safe_id.is_empty() { "session".to_string() } else { safe_id };
        let tmp = std::env::temp_dir().join(format!("claurst-session-{stem}.html"));

        if let Err(e) = write_session_html(&tmp, &ctx.messages, &meta) {
            return CommandResult::Error(format!("Failed to render session HTML: {e}"));
        }

        tracing::info!(target: "share", path = %tmp.display(), "Uploading session HTML as secret gist");

        // 3. Upload as a secret gist (async, so the TUI stays responsive).
        let result = tokio::process::Command::new("gh")
            .args(["gist", "create", "--public=false"])
            .arg(&tmp)
            .output()
            .await;

        // Best-effort tmp cleanup.
        let _ = std::fs::remove_file(&tmp);

        let output = match result {
            Ok(o) => o,
            Err(e) => return CommandResult::Error(format!("Failed to spawn gh: {e}")),
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let msg = stderr.trim();
            return CommandResult::Error(format!(
                "gh gist create failed: {}",
                if msg.is_empty() { "unknown error" } else { msg }
            ));
        }

        // 4. Parse gist URL and derive the viewer URL.
        let stdout = String::from_utf8_lossy(&output.stdout);
        let gist_url = stdout.trim();
        let gist_id = gist_url.rsplit('/').next().unwrap_or("").trim();
        if gist_id.is_empty() {
            return CommandResult::Error(format!(
                "Could not parse gist id from gh output: {gist_url:?}"
            ));
        }
        let viewer = share_viewer_url(gist_id);

        // Auto-open in the system browser unless the user opted out — saves the
        // copy/paste dance after a /share. Skipped when `CLAURST_SHARE_NO_OPEN`
        // is set (e.g. on a headless box) or when `open` can't find a handler.
        let opted_out = std::env::var_os("CLAURST_SHARE_NO_OPEN")
            .map(|v| !v.is_empty() && v != "0")
            .unwrap_or(false);
        let opened = if opted_out {
            false
        } else {
            open::that(&viewer).is_ok()
        };

        let footer = if opened {
            "Opened in your browser. The gist is secret (unlisted); delete it to revoke access."
        } else if opted_out {
            "The gist is secret (unlisted). Anyone with the link can view it; delete the gist to revoke access."
        } else {
            "Could not auto-open the link. Copy the URL above. The gist is secret (unlisted); delete the gist to revoke access."
        };

        CommandResult::Message(format!(
            "Share URL: {viewer}\nGist: {gist_url}\n\n{footer}"
        ))
    }
}

// ---- /links --------------------------------------------------------------

/// Detect URLs in plain text. Mirrors the styling regex in tui::messages::markdown
/// so the user sees the same links the renderer highlights.
fn links_url_regex() -> &'static regex::Regex {
    static URL_RE: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
        regex::Regex::new(r"(?:https?|ftp)://\S+|www\.\S+").expect("links URL regex")
    });
    &URL_RE
}

fn strip_trailing_punct(url: &str) -> String {
    let mut s = url.to_string();
    while let Some(c) = s.chars().last() {
        if matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '\'' | '"' | '>') {
            s.pop();
        } else {
            break;
        }
    }
    s
}

/// Walk messages (oldest → newest), pulling text out of each block and
/// returning unique URLs in *most-recent-first* order.
fn extract_session_urls(messages: &[Message]) -> Vec<String> {
    let re = links_url_regex();
    let mut ordered: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for msg in messages {
        let text: String = match &msg.content {
            claurst_core::types::MessageContent::Text(t) => t.clone(),
            claurst_core::types::MessageContent::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        };
        for m in re.find_iter(&text) {
            let url = strip_trailing_punct(m.as_str());
            if !url.is_empty() && seen.insert(url.clone()) {
                ordered.push(url);
            }
        }
    }
    // Most-recent first.
    ordered.reverse();
    ordered
}

#[async_trait]
impl SlashCommand for LinksCommand {
    fn name(&self) -> &str { "links" }
    fn aliases(&self) -> Vec<&str> { vec!["link"] }
    fn description(&self) -> &str {
        "List URLs in this session and open them in your browser"
    }
    fn help(&self) -> &str {
        "Usage: /links [N | last | list]\n\n\
         /links            Open the most recent URL in your browser.\n\
         /links list       Print a numbered list of URLs (most recent first).\n\
         /links <N>        Open the Nth URL from /links list.\n\
         /links last       Same as /links (open most recent).\n\n\
         URLs are detected in user/assistant message text. Set\n\
         CLAURST_SHARE_NO_OPEN=1 to disable the auto-open behavior in /share."
    }

    async fn execute(&self, args: &str, ctx: &mut CommandContext) -> CommandResult {
        let urls = extract_session_urls(&ctx.messages);
        if urls.is_empty() {
            return CommandResult::Message("No URLs found in this session yet.".to_string());
        }

        let arg = args.trim();

        // /links list -> print numbered list, don't open anything.
        if arg.eq_ignore_ascii_case("list") {
            let mut out = format!("URLs in this session ({}):\n", urls.len());
            for (i, u) in urls.iter().enumerate() {
                out.push_str(&format!("  {}. {}\n", i + 1, u));
            }
            out.push_str("\nRun /links <N> to open one in your browser.");
            return CommandResult::Message(out);
        }

        // Resolve which URL to open.
        let target = if arg.is_empty() || arg.eq_ignore_ascii_case("last") {
            &urls[0]
        } else {
            match arg.parse::<usize>() {
                Ok(n) if (1..=urls.len()).contains(&n) => &urls[n - 1],
                Ok(_) => {
                    return CommandResult::Error(format!(
                        "Index out of range. There are {} URLs — try /links list.",
                        urls.len()
                    ));
                }
                Err(_) => {
                    return CommandResult::Error(
                        "Usage: /links [N | last | list]. Run /links list to see indices."
                            .to_string(),
                    );
                }
            }
        };

        match open::that(target) {
            Ok(_) => CommandResult::Message(format!("Opening {} in your browser…", target)),
            Err(e) => CommandResult::Error(format!(
                "Could not open {}: {}. Copy it manually:\n{}",
                target, e, target
            )),
        }
    }
}
