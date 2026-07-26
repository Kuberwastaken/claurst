// `/chrome` command and its CDP client (`chrome_cdp`).
//
// Extracted from lib.rs (issue #232). Behavior-preserving move.

use super::*;
use async_trait::async_trait;

pub struct ChromeCommand;

// ---- /chrome -------------------------------------------------------------
//
// Real CDP-over-WebSocket implementation.
//
// Chrome must be launched with:
//   chrome --remote-debugging-port=9222 --no-first-run
//
// The connection is stored in a process-wide lazy mutex so subsequent
// subcommand calls reuse the same WebSocket session.

mod chrome_cdp {
    use base64::Engine as _;
    use once_cell::sync::Lazy;
    use parking_lot::Mutex;
    use serde_json::{json, Value};
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::net::TcpStream;
    use tokio_tungstenite::{
        connect_async, tungstenite::Message as WsMessage, MaybeTlsStream, WebSocketStream,
    };
    use futures::{SinkExt, StreamExt};

    // -----------------------------------------------------------------------
    // Global session state
    // -----------------------------------------------------------------------

    #[allow(dead_code)]
    pub struct ChromeSession {
        pub ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
        pub port: u16,
        pub tab_url: String,
    }

    static SESSION: Lazy<Mutex<Option<ChromeSession>>> = Lazy::new(|| Mutex::new(None));
    static MSG_ID: AtomicU64 = AtomicU64::new(1);

    fn next_id() -> u64 {
        MSG_ID.fetch_add(1, Ordering::Relaxed)
    }

    // -----------------------------------------------------------------------
    // Low-level CDP helpers
    // -----------------------------------------------------------------------

    /// Send a CDP method call and wait for the matching response.
    /// Returns the full response object (including `result` / `error`).
    async fn cdp_call(
        ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
        method: &str,
        params: Value,
    ) -> anyhow::Result<Value> {
        let id = next_id();
        let request = json!({ "id": id, "method": method, "params": params });
        ws.send(WsMessage::Text(request.to_string())).await?;

        // Drain messages until we get the one with our id (ignore events).
        loop {
            let raw = ws
                .next()
                .await
                .ok_or_else(|| anyhow::anyhow!("WebSocket closed unexpectedly"))??;
            let text: String = match raw {
                WsMessage::Text(t) => t.to_string(),
                WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
                WsMessage::Close(_) => {
                    return Err(anyhow::anyhow!("WebSocket closed by Chrome"));
                }
                _ => continue,
            };
            let val: Value = serde_json::from_str(&text)?;
            if val["id"] == id {
                if let Some(err) = val.get("error") {
                    return Err(anyhow::anyhow!("CDP error: {}", err));
                }
                return Ok(val);
            }
            // It's an event or different response — keep waiting.
        }
    }

    // -----------------------------------------------------------------------
    // Session take/restore helpers
    //
    // We avoid holding a MutexGuard across await points by taking ownership
    // of the session, performing all async operations with it, then putting
    // it back into the global.
    // -----------------------------------------------------------------------

    fn take_session() -> anyhow::Result<ChromeSession> {
        SESSION.lock().take().ok_or_else(|| {
            anyhow::anyhow!("No active Chrome session. Run `/chrome connect` first.")
        })
    }

    fn store_session(s: ChromeSession) {
        *SESSION.lock() = Some(s);
    }

    // -----------------------------------------------------------------------
    // Public helpers called from the SlashCommand impl
    // -----------------------------------------------------------------------

    /// Connect to Chrome at the given port.
    /// Picks the first available target (tab/page).
    pub async fn connect(port: u16) -> anyhow::Result<String> {
        let http_url = format!("http://localhost:{}/json/list", port);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()?;
        let tabs: Value = client.get(&http_url).send().await?.json().await?;

        let ws_url = tabs
            .as_array()
            .and_then(|arr| {
                arr.iter().find(|t| t["type"] == "page").and_then(|t| {
                    t["webSocketDebuggerUrl"].as_str().map(|s| s.to_string())
                })
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No debuggable page found on port {}. \
                     Make sure Chrome has at least one open tab.",
                    port
                )
            })?;

        let tab_url = tabs
            .as_array()
            .and_then(|arr| {
                arr.iter()
                    .find(|t| t["type"] == "page")
                    .and_then(|t| t["url"].as_str().map(|s| s.to_string()))
            })
            .unwrap_or_default();

        let (ws, _) = connect_async(&ws_url).await.map_err(|e| {
            anyhow::anyhow!("WebSocket connect to {} failed: {}", ws_url, e)
        })?;

        let mut session = ChromeSession { ws, port, tab_url: tab_url.clone() };
        // Enable Page domain so captureScreenshot etc. work.
        cdp_call(&mut session.ws, "Page.enable", json!({})).await?;
        // Enable Runtime domain for eval/click/fill.
        cdp_call(&mut session.ws, "Runtime.enable", json!({})).await?;

        store_session(session);

        Ok(format!(
            "Connected to Chrome on port {} (tab: {})",
            port, tab_url
        ))
    }

    /// Disconnect the current session.
    pub fn disconnect() -> String {
        let mut guard = SESSION.lock();
        if guard.is_some() {
            *guard = None;
            "Disconnected from Chrome.".to_string()
        } else {
            "No active Chrome session.".to_string()
        }
    }

    /// Navigate to a URL.
    pub async fn navigate(url: &str) -> anyhow::Result<String> {
        let url = url.to_string();
        let mut s = take_session()?;
        let result = async {
            let resp = cdp_call(&mut s.ws, "Page.navigate", json!({ "url": url })).await?;
            let frame_id = resp["result"]["frameId"].as_str().unwrap_or("unknown");
            Ok(format!("Navigated. frameId={}", frame_id))
        }
        .await;
        store_session(s);
        result
    }

    /// Take a screenshot, write PNG to a temp file, return the path.
    pub async fn screenshot() -> anyhow::Result<String> {
        let mut s = take_session()?;
        let result = async {
            let resp = cdp_call(
                &mut s.ws,
                "Page.captureScreenshot",
                json!({ "format": "png", "captureBeyondViewport": false }),
            )
            .await?;
            let b64 = resp["result"]["data"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("No screenshot data in response"))?;
            let bytes = base64::engine::general_purpose::STANDARD.decode(b64)?;

            let tmp = tempfile::Builder::new()
                .prefix("cc-chrome-")
                .suffix(".png")
                .tempfile()?;
            let path = tmp.path().to_path_buf();
            std::fs::write(&path, &bytes)?;
            // Persist file past the NamedTempFile drop.
            let _ = tmp.keep()?;
            Ok(format!("Screenshot saved to {}", path.display()))
        }
        .await;
        store_session(s);
        result
    }

    /// Click the first element matching a CSS selector.
    pub async fn click(selector: &str) -> anyhow::Result<String> {
        let sel_json = serde_json::to_string(selector)?;
        let js = format!(
            r#"(function(){{
                var el=document.querySelector({sel});
                if(!el)return 'ELEMENT_NOT_FOUND';
                var r=el.getBoundingClientRect();
                return JSON.stringify({{x:r.left+r.width/2,y:r.top+r.height/2}});
            }})()"#,
            sel = sel_json
        );
        let selector = selector.to_string();
        let mut s = take_session()?;
        let result = async {
            let resp = cdp_call(
                &mut s.ws,
                "Runtime.evaluate",
                json!({ "expression": js, "returnByValue": true }),
            )
            .await?;
            let val_str = resp["result"]["result"]["value"].as_str().unwrap_or("");
            if val_str == "ELEMENT_NOT_FOUND" {
                return Err(anyhow::anyhow!(
                    "No element found for selector: {}",
                    selector
                ));
            }
            let coords: Value = serde_json::from_str(val_str)?;
            let x = coords["x"].as_f64().unwrap_or(0.0);
            let y = coords["y"].as_f64().unwrap_or(0.0);

            cdp_call(
                &mut s.ws,
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mousePressed", "x": x, "y": y,
                    "button": "left", "clickCount": 1
                }),
            )
            .await?;
            cdp_call(
                &mut s.ws,
                "Input.dispatchMouseEvent",
                json!({
                    "type": "mouseReleased", "x": x, "y": y,
                    "button": "left", "clickCount": 1
                }),
            )
            .await?;

            Ok(format!("Clicked '{}' at ({:.0}, {:.0})", selector, x, y))
        }
        .await;
        store_session(s);
        result
    }

    /// Fill an input field.
    pub async fn fill(selector: &str, text: &str) -> anyhow::Result<String> {
        let js = format!(
            r#"(function(){{
                var el=document.querySelector({sel});
                if(!el)return false;
                el.focus();
                el.value={val};
                el.dispatchEvent(new Event('input',{{bubbles:true}}));
                el.dispatchEvent(new Event('change',{{bubbles:true}}));
                return true;
            }})()"#,
            sel = serde_json::to_string(selector)?,
            val = serde_json::to_string(text)?
        );
        let selector = selector.to_string();
        let text = text.to_string();
        let mut s = take_session()?;
        let result = async {
            let resp = cdp_call(
                &mut s.ws,
                "Runtime.evaluate",
                json!({ "expression": js, "returnByValue": true }),
            )
            .await?;
            let ok = resp["result"]["result"]["value"].as_bool().unwrap_or(false);
            if ok {
                Ok(format!("Filled '{}' with {:?}", selector, text))
            } else {
                Err(anyhow::anyhow!(
                    "No element found for selector: {}",
                    selector
                ))
            }
        }
        .await;
        store_session(s);
        result
    }

    /// Evaluate arbitrary JavaScript and return the result as a string.
    pub async fn eval(js: &str) -> anyhow::Result<String> {
        let js = js.to_string();
        let mut s = take_session()?;
        let result = async {
            let resp = cdp_call(
                &mut s.ws,
                "Runtime.evaluate",
                json!({ "expression": js, "returnByValue": true }),
            )
            .await?;
            let result_val = &resp["result"]["result"];
            let out = if let Some(v) = result_val["value"].as_str() {
                v.to_string()
            } else if !result_val["value"].is_null() {
                result_val["value"].to_string()
            } else if let Some(desc) = result_val["description"].as_str() {
                desc.to_string()
            } else {
                result_val.to_string()
            };
            Ok(out)
        }
        .await;
        store_session(s);
        result
    }

}

// ---- SlashCommand impl -------------------------------------------------------

#[async_trait]
impl SlashCommand for ChromeCommand {
    fn name(&self) -> &str { "chrome" }
    fn description(&self) -> &str {
        "Browser automation via Chrome DevTools Protocol (CDP)"
    }
    fn help(&self) -> &str {
        "Usage: /chrome <subcommand> [args]\n\n\
         Control a running Chrome/Chromium browser via CDP.\n\n\
         First, launch Chrome with remote debugging enabled:\n\
           chrome --remote-debugging-port=9222 --no-first-run\n\n\
         Subcommands:\n\
           /chrome connect [--port 9222]      — connect to Chrome\n\
           /chrome navigate <url>             — navigate to URL\n\
           /chrome screenshot                 — take screenshot, save to temp file\n\
           /chrome click <selector>           — click CSS selector\n\
           /chrome fill <selector> <text>     — fill input field\n\
           /chrome eval <js>                  — evaluate JavaScript\n\
           /chrome disconnect                 — disconnect"
    }

    async fn execute(&self, args: &str, _ctx: &mut CommandContext) -> CommandResult {
        let mut parts = args.trim().splitn(2, char::is_whitespace);
        let sub = parts.next().unwrap_or("").trim();
        let rest = parts.next().unwrap_or("").trim();

        match sub {
            // ------------------------------------------------------------------
            // /chrome connect [--port <N>]
            // ------------------------------------------------------------------
            "connect" => {
                let port: u16 = if let Some(p) = rest.strip_prefix("--port ").map(str::trim) {
                    match p.parse() {
                        Ok(n) => n,
                        Err(_) => {
                            return CommandResult::Error(format!(
                                "Invalid port number: {}",
                                p
                            ));
                        }
                    }
                } else if rest.is_empty() {
                    9222
                } else {
                    match rest.parse() {
                        Ok(n) => n,
                        Err(_) => {
                            return CommandResult::Error(format!(
                                "Usage: /chrome connect [--port <N>]\nInvalid argument: {}",
                                rest
                            ));
                        }
                    }
                };

                match chrome_cdp::connect(port).await {
                    Ok(msg) => CommandResult::Message(msg),
                    Err(e) => CommandResult::Error(format!(
                        "Failed to connect to Chrome on port {}: {}\n\n\
                         Make sure Chrome is running with:\n\
                           chrome --remote-debugging-port={} --no-first-run",
                        port, e, port
                    )),
                }
            }

            // ------------------------------------------------------------------
            // /chrome navigate <url>
            // ------------------------------------------------------------------
            "navigate" => {
                if rest.is_empty() {
                    return CommandResult::Error(
                        "Usage: /chrome navigate <url>\nExample: /chrome navigate https://example.com"
                            .to_string(),
                    );
                }
                match chrome_cdp::navigate(rest).await {
                    Ok(msg) => CommandResult::Message(msg),
                    Err(e) => CommandResult::Error(e.to_string()),
                }
            }

            // ------------------------------------------------------------------
            // /chrome screenshot
            // ------------------------------------------------------------------
            "screenshot" => match chrome_cdp::screenshot().await {
                Ok(msg) => CommandResult::Message(msg),
                Err(e) => CommandResult::Error(e.to_string()),
            },

            // ------------------------------------------------------------------
            // /chrome click <selector>
            // ------------------------------------------------------------------
            "click" => {
                if rest.is_empty() {
                    return CommandResult::Error(
                        "Usage: /chrome click <css-selector>\nExample: /chrome click button#submit"
                            .to_string(),
                    );
                }
                match chrome_cdp::click(rest).await {
                    Ok(msg) => CommandResult::Message(msg),
                    Err(e) => CommandResult::Error(e.to_string()),
                }
            }

            // ------------------------------------------------------------------
            // /chrome fill <selector> <text>
            // ------------------------------------------------------------------
            "fill" => {
                // Split selector and text at first whitespace.
                let mut fill_parts = rest.splitn(2, char::is_whitespace);
                let selector = fill_parts.next().unwrap_or("").trim();
                let text = fill_parts.next().unwrap_or("").trim();
                if selector.is_empty() {
                    return CommandResult::Error(
                        "Usage: /chrome fill <css-selector> <text>\nExample: /chrome fill input#email user@example.com"
                            .to_string(),
                    );
                }
                match chrome_cdp::fill(selector, text).await {
                    Ok(msg) => CommandResult::Message(msg),
                    Err(e) => CommandResult::Error(e.to_string()),
                }
            }

            // ------------------------------------------------------------------
            // /chrome eval <js>
            // ------------------------------------------------------------------
            "eval" => {
                if rest.is_empty() {
                    return CommandResult::Error(
                        "Usage: /chrome eval <javascript>\nExample: /chrome eval document.title"
                            .to_string(),
                    );
                }
                match chrome_cdp::eval(rest).await {
                    Ok(result) => CommandResult::Message(format!("=> {}", result)),
                    Err(e) => CommandResult::Error(e.to_string()),
                }
            }

            // ------------------------------------------------------------------
            // /chrome disconnect
            // ------------------------------------------------------------------
            "disconnect" => CommandResult::Message(chrome_cdp::disconnect()),

            // ------------------------------------------------------------------
            // No subcommand or unknown
            // ------------------------------------------------------------------
            "" => CommandResult::Message(self.help().to_string()),
            other => CommandResult::Error(format!(
                "Unknown subcommand: '{}'\n\n{}",
                other,
                self.help()
            )),
        }
    }
}
