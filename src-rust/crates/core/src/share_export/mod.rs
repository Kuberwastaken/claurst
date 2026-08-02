// share_export: Render a Claurst session as a single self-contained HTML file
// for the `/share` slash command.
//
// The output is one HTML document with:
//   * inline CSS (template.css)
//   * inline JS  (template.js) — client-side message renderer
//   * the session payload (meta + messages) embedded as base64-encoded JSON
//     in a <script id="session-data"> tag
//   * marked + highlight.js loaded from jsdelivr (the viewer is hosted, so the
//     network is already required to reach it)
//
// Used by `cc-commands::ShareCommand`, which uploads the resulting file as a
// secret GitHub gist via the `gh` CLI and constructs a viewer URL of the form
// `https://claurst.kuber.studio/session/#<gist-id>` (overridable via
// `CLAURST_SHARE_VIEWER_URL`).

use std::path::Path;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::Serialize;

use crate::types::Message;

const TEMPLATE_HTML: &str = include_str!("template.html");
const TEMPLATE_CSS: &str = include_str!("template.css");
const TEMPLATE_JS: &str = include_str!("template.js");

/// Default viewer base URL used when `CLAURST_SHARE_VIEWER_URL` is unset.
pub const DEFAULT_SHARE_VIEWER_URL: &str = "https://claurst.kuber.studio/session/";

/// Environment variable that overrides the share viewer base URL.
pub const ENV_SHARE_VIEWER_URL: &str = "CLAURST_SHARE_VIEWER_URL";

/// Metadata recorded alongside the message stream.
#[derive(Debug, Clone, Serialize)]
pub struct SessionExportMeta {
    pub session_id: String,
    pub title: Option<String>,
    pub model: String,
    pub working_dir: String,
    pub exported_at: String,
    pub app_version: String,
}

#[derive(Debug, Serialize)]
struct SessionPayload<'a> {
    meta: &'a SessionExportMeta,
    messages: &'a [Message],
}

/// Render the session as a complete, standalone HTML document.
pub fn render_session_html(messages: &[Message], meta: &SessionExportMeta) -> String {
    let payload = SessionPayload { meta, messages };
    let json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
    let b64 = STANDARD.encode(json.as_bytes());

    let title = meta
        .title
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| format!("Session {}", meta.session_id));
    let title_escaped = html_escape(&title);

    TEMPLATE_HTML
        .replace("{{TITLE}}", &title_escaped)
        .replace("{{CSS}}", TEMPLATE_CSS)
        .replace("{{JS}}", TEMPLATE_JS)
        .replace("{{SESSION_DATA}}", &b64)
}

/// Render and write the session HTML to `path`.
pub fn write_session_html(
    path: &Path,
    messages: &[Message],
    meta: &SessionExportMeta,
) -> std::io::Result<()> {
    let html = render_session_html(messages, meta);
    std::fs::write(path, html)
}

/// Build the viewer URL for a given gist id, honoring `CLAURST_SHARE_VIEWER_URL`.
pub fn share_viewer_url(gist_id: &str) -> String {
    let base = std::env::var(ENV_SHARE_VIEWER_URL)
        .unwrap_or_else(|_| DEFAULT_SHARE_VIEWER_URL.to_string());
    if base.ends_with('/') {
        format!("{base}#{gist_id}")
    } else {
        format!("{base}/#{gist_id}")
    }
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Message, MessageContent, Role};

    fn meta() -> SessionExportMeta {
        SessionExportMeta {
            session_id: "abc12345".to_string(),
            title: Some("My session".to_string()),
            model: "claude-sonnet-4-6".to_string(),
            working_dir: "/tmp/proj".to_string(),
            exported_at: "2026-05-20T12:00:00Z".to_string(),
            app_version: "0.1.3".to_string(),
        }
    }

    #[test]
    fn renders_full_html_document() {
        let msgs = vec![Message {
            role: Role::User,
            content: MessageContent::Text("hello".to_string()),
            uuid: None,
            cost: None,
            snapshot_patch: None,
        }];
        let html = render_session_html(&msgs, &meta());
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<title>My session</title>"));
        assert!(html.contains("id=\"session-data\""));
        assert!(!html.contains("{{TITLE}}"));
        assert!(!html.contains("{{CSS}}"));
        assert!(!html.contains("{{JS}}"));
        assert!(!html.contains("{{SESSION_DATA}}"));
    }

    #[test]
    fn falls_back_to_session_id_when_title_blank() {
        let mut m = meta();
        m.title = Some("   ".to_string());
        let html = render_session_html(&[], &m);
        assert!(html.contains("<title>Session abc12345</title>"));
    }

    #[test]
    fn html_escape_handles_specials() {
        assert_eq!(
            html_escape("<a href=\"x\">&amp;</a>"),
            "&lt;a href=&quot;x&quot;&gt;&amp;amp;&lt;/a&gt;"
        );
    }

    // Both viewer-url scenarios live in one test because they manipulate a
    // process-wide env var; running them in parallel would race.
    #[test]
    fn viewer_url_default_and_override() {
        std::env::remove_var(ENV_SHARE_VIEWER_URL);
        assert_eq!(
            share_viewer_url("deadbeef"),
            "https://claurst.kuber.studio/session/#deadbeef"
        );

        std::env::set_var(ENV_SHARE_VIEWER_URL, "https://example.test/v");
        assert_eq!(share_viewer_url("xyz"), "https://example.test/v/#xyz");

        std::env::set_var(ENV_SHARE_VIEWER_URL, "https://example.test/v/");
        assert_eq!(share_viewer_url("xyz"), "https://example.test/v/#xyz");

        std::env::remove_var(ENV_SHARE_VIEWER_URL);
    }
}
