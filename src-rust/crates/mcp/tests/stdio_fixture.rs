// End-to-end tests for `McpManager` against a fake stdio MCP server.
//
// This target has `harness = false`: the same binary is both the test runner
// and the fake server. When `FAKE_MCP_MODE` is set it speaks newline-delimited
// JSON-RPC on stdin/stdout instead of running tests, and the tests spawn
// `current_exe()` with that variable as a real stdio MCP server. No libtest
// output can corrupt the protocol stream, and no extra binary is shipped.

use claurst_core::config::McpServerConfig;
use claurst_core::types::ContentBlock;
use claurst_mcp::{mcp_result_to_output, McpContent, McpManager, McpServerStatus};
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    if let Ok(mode) = std::env::var("FAKE_MCP_MODE") {
        fake_server::run(&mode);
        return;
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let mut failed = 0usize;
    let mut run = |name: &str, test: std::pin::Pin<Box<dyn Future<Output = Result<(), String>>>>| {
        let outcome = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(60), test)
                .await
                .unwrap_or_else(|_| Err("test timed out after 60s".to_string()))
        });
        match outcome {
            Ok(()) => println!("test {name} ... ok"),
            Err(e) => {
                failed += 1;
                println!("test {name} ... FAILED\n    {e}");
            }
        }
    };

    run("hung_server_times_out", Box::pin(hung_server_times_out()));
    run(
        "servers_connect_concurrently_without_blocking_start",
        Box::pin(servers_connect_concurrently_without_blocking_start()),
    );
    run(
        "exposed_names_are_sanitized_and_routed_by_map",
        Box::pin(exposed_names_are_sanitized_and_routed_by_map()),
    );
    run("list_changed_refreshes_tools", Box::pin(list_changed_refreshes_tools()));
    run(
        "server_instructions_reach_the_prompt_section",
        Box::pin(server_instructions_reach_the_prompt_section()),
    );
    run(
        "structured_content_and_images_are_converted",
        Box::pin(structured_content_and_images_are_converted()),
    );

    if failed > 0 {
        println!("\n{failed} stdio fixture test(s) failed");
        std::process::exit(1);
    }
    println!("\nall stdio fixture tests passed");
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A config that launches this binary as a fake server in `mode`.
fn fake_server(name: &str, mode: &str, extra_env: &[(&str, &str)]) -> McpServerConfig {
    let exe = std::env::current_exe().expect("current_exe");
    let mut env = HashMap::new();
    env.insert("FAKE_MCP_MODE".to_string(), mode.to_string());
    env.insert("FAKE_MCP_NAME".to_string(), name.to_string());
    for (key, value) in extra_env {
        env.insert((*key).to_string(), (*value).to_string());
    }
    McpServerConfig {
        name: name.to_string(),
        command: Some(exe.to_string_lossy().into_owned()),
        args: vec![],
        env,
        url: None,
        server_type: "stdio".to_string(),
        headers: Default::default(),
        connect_timeout_ms: None,
        catalog_timeout_ms: None,
        origin: Default::default(),
    }
}

/// Poll `condition` until it holds or `within` elapses.
async fn wait_for(within: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    condition()
}

fn ensure(condition: bool, message: impl FnOnce() -> String) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message())
    }
}

fn is_connected(manager: &Arc<McpManager>, name: &str) -> bool {
    matches!(manager.server_status(name), McpServerStatus::Connected { .. })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

async fn hung_server_times_out() -> Result<(), String> {
    let mut config = fake_server("hung", "hang", &[]);
    config.connect_timeout_ms = Some(500);
    let started = Instant::now();
    let manager = McpManager::start(&[config]);
    manager.wait_until_settled().await;
    let elapsed = started.elapsed();

    ensure(elapsed < Duration::from_secs(5), || {
        format!("settling took {elapsed:?}, expected the 500 ms connect timeout to apply")
    })?;
    match manager.server_status("hung") {
        McpServerStatus::Disconnected { last_error: Some(error) } => ensure(
            error.contains("timed out"),
            || format!("expected a timeout error, got: {error}"),
        ),
        other => Err(format!("expected a failed server, got {other:?}")),
    }
}

async fn servers_connect_concurrently_without_blocking_start() -> Result<(), String> {
    let mut hung = fake_server("slow", "hang", &[]);
    hung.connect_timeout_ms = Some(3_000);
    let healthy = fake_server("quick", "serve", &[]);

    let started = Instant::now();
    let manager = McpManager::start(&[hung, healthy]);
    let start_cost = started.elapsed();
    ensure(start_cost < Duration::from_millis(500), || {
        format!("start() blocked for {start_cost:?}")
    })?;
    ensure(
        matches!(manager.server_status("slow"), McpServerStatus::Connecting),
        || format!("expected 'slow' to be connecting, got {:?}", manager.server_status("slow")),
    )?;

    // The healthy server must not wait behind the hung one.
    let quick_up = wait_for(Duration::from_millis(2_500), || is_connected(&manager, "quick")).await;
    ensure(quick_up, || {
        format!("'quick' did not connect while 'slow' hung: {:?}", manager.server_status("quick"))
    })?;
    ensure(
        matches!(manager.server_status("slow"), McpServerStatus::Connecting),
        || format!("'slow' should still be connecting, got {:?}", manager.server_status("slow")),
    )?;

    manager.wait_until_settled().await;
    ensure(manager.failed_servers().len() == 1, || {
        format!("expected exactly one failed server, got {:?}", manager.failed_servers())
    })
}

async fn exposed_names_are_sanitized_and_routed_by_map() -> Result<(), String> {
    // With `{server}_{tool}` naming plus prefix stripping, `git`/`lab_status`
    // and `git_lab`/`status` both became `git_lab_status`.
    let manager = McpManager::connect_all(&[
        fake_server("git", "serve", &[("FAKE_MCP_TOOLS", "lab_status")]),
        fake_server("git_lab", "serve", &[("FAKE_MCP_TOOLS", "status")]),
        fake_server("my.server", "serve", &[("FAKE_MCP_TOOLS", "do it")]),
    ])
    .await;

    let mut names: Vec<String> = manager
        .all_tool_definitions()
        .into_iter()
        .map(|entry| entry.definition.name)
        .collect();
    names.sort();
    ensure(
        names == ["mcp__git__lab_status", "mcp__git_lab__status", "mcp__my_server__do_it"],
        || format!("unexpected exposed names: {names:?}"),
    )?;

    for (exposed, expected) in [
        ("mcp__git__lab_status", "server=git tool=lab_status"),
        ("mcp__git_lab__status", "server=git_lab tool=status"),
        ("mcp__my_server__do_it", "server=my.server tool=do it"),
    ] {
        let result = manager
            .call_tool(exposed, Some(serde_json::json!({})))
            .await
            .map_err(|e| format!("calling {exposed}: {e}"))?;
        let text = match result.content.first() {
            Some(McpContent::Text { text }) => text.clone(),
            other => return Err(format!("{exposed}: unexpected content {other:?}")),
        };
        ensure(text == expected, || format!("{exposed} routed to the wrong tool: {text}"))?;
    }

    ensure(manager.call_tool("git_lab_status", None).await.is_err(), || {
        "the legacy name must not route".to_string()
    })
}

fn exposed_names(manager: &McpManager) -> Vec<String> {
    let mut names: Vec<String> = manager
        .all_tool_definitions()
        .into_iter()
        .map(|entry| entry.definition.name)
        .collect();
    names.sort();
    names
}

async fn list_changed_refreshes_tools() -> Result<(), String> {
    let manager =
        McpManager::connect_all(&[fake_server("dyn", "serve", &[("FAKE_MCP_TOOLS", "echo,add_tool")])]).await;
    let before = exposed_names(&manager);
    ensure(before == ["mcp__dyn__add_tool", "mcp__dyn__echo"], || {
        format!("unexpected initial tools: {before:?}")
    })?;
    let generation = manager.generation();

    // `add_tool` makes the server add `late_tool` and send
    // `notifications/tools/list_changed`.
    manager
        .call_tool("mcp__dyn__add_tool", Some(serde_json::json!({})))
        .await
        .map_err(|e| format!("add_tool: {e}"))?;
    let refreshed = wait_for(Duration::from_secs(10), || {
        exposed_names(&manager).iter().any(|name| name == "mcp__dyn__late_tool")
    })
    .await;
    ensure(refreshed, || {
        format!("tool list was not refreshed: {:?}", exposed_names(&manager))
    })?;
    ensure(manager.generation() != generation, || {
        "a refresh must bump the generation".to_string()
    })?;
    ensure(
        matches!(manager.server_status("dyn"), McpServerStatus::Connected { tool_count: 3 }),
        || format!("status not updated: {:?}", manager.server_status("dyn")),
    )?;

    let result = manager
        .call_tool("mcp__dyn__late_tool", Some(serde_json::json!({})))
        .await
        .map_err(|e| format!("late_tool: {e}"))?;
    ensure(mcp_result_to_output(&result).text == "server=dyn tool=late_tool", || {
        "late_tool must route after the refresh".to_string()
    })
}

async fn server_instructions_reach_the_prompt_section() -> Result<(), String> {
    let manager = McpManager::connect_all(&[
        fake_server(
            "docs",
            "serve",
            &[("FAKE_MCP_INSTRUCTIONS", "Search before you answer; cite page URLs.")],
        ),
        fake_server("plain", "serve", &[]),
    ])
    .await;

    let section = manager
        .instructions_prompt_section(|name| name.starts_with("mcp__"))
        .ok_or("expected an instructions section")?;
    ensure(
        section.contains("## docs\nSearch before you answer; cite page URLs."),
        || format!("instructions missing from section:\n{section}"),
    )?;
    ensure(!section.contains("## plain"), || {
        format!("a server without instructions must not appear:\n{section}")
    })?;
    ensure(
        manager.instructions_prompt_section(|name| name == "mcp__plain__echo").is_none(),
        || "instructions must only be injected for servers whose tools are active".to_string(),
    )
}

async fn structured_content_and_images_are_converted() -> Result<(), String> {
    let manager =
        McpManager::connect_all(&[fake_server("rich", "serve", &[("FAKE_MCP_TOOLS", "structured,image")])]).await;

    let structured = manager
        .call_tool("mcp__rich__structured", Some(serde_json::json!({})))
        .await
        .map_err(|e| format!("structured: {e}"))?;
    let output = mcp_result_to_output(&structured);
    ensure(output.text == r#"{"answer":42}"#, || {
        format!("structuredContent should replace the text blocks, got {:?}", output.text)
    })?;

    let image = manager
        .call_tool("mcp__rich__image", Some(serde_json::json!({})))
        .await
        .map_err(|e| format!("image: {e}"))?;
    let output = mcp_result_to_output(&image);
    ensure(output.text == "chart\n[Image: image/png, 8 B]", || {
        format!("unexpected image placeholder text {:?}", output.text)
    })?;
    match output.images.as_slice() {
        [ContentBlock::Image { source }] => ensure(
            source.data.as_deref() == Some("iVBORw0KGgo=") && source.media_type.as_deref() == Some("image/png"),
            || format!("unexpected image source {source:?}"),
        ),
        other => Err(format!("expected one image block, got {other:?}")),
    }
}

// ---------------------------------------------------------------------------
// Fake stdio MCP server
// ---------------------------------------------------------------------------

mod fake_server {
    use serde_json::{json, Value};
    use std::io::{BufRead, Write};

    fn send(out: &mut impl Write, message: &Value) {
        // A closed stdout means the client went away; nothing left to do.
        if writeln!(out, "{message}").and_then(|_| out.flush()).is_err() {
            std::process::exit(0);
        }
    }

    /// `hang`: read requests and never answer, like a server stuck at startup.
    /// `serve`: a minimal, well-behaved server. `FAKE_MCP_TOOLS` lists its
    /// tools; calling `add_tool` adds `late_tool` and announces the change.
    pub fn run(mode: &str) {
        let name = std::env::var("FAKE_MCP_NAME").unwrap_or_else(|_| "fake".to_string());
        let tool_names: Vec<String> = std::env::var("FAKE_MCP_TOOLS")
            .unwrap_or_else(|_| "echo".to_string())
            .split(',')
            .map(str::to_string)
            .collect();
        let mut tool_names = tool_names;
        let instructions = std::env::var("FAKE_MCP_INSTRUCTIONS").ok();
        let stdin = std::io::stdin();
        let mut stdout = std::io::stdout().lock();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if mode == "hang" {
                continue;
            }
            let Ok(request) = serde_json::from_str::<Value>(&line) else { continue };
            let Some(id) = request.get("id").cloned() else { continue };
            let method = request["method"].as_str().unwrap_or_default();
            let result = match method {
                "initialize" => {
                    let mut result = json!({
                        "protocolVersion": request["params"]["protocolVersion"],
                        "capabilities": { "tools": { "listChanged": true } },
                        "serverInfo": { "name": name, "version": "0.0.0" },
                    });
                    if let Some(instructions) = &instructions {
                        result["instructions"] = json!(instructions);
                    }
                    result
                }
                "tools/list" => {
                    let tools: Vec<Value> = tool_names
                        .iter()
                        .map(|tool| {
                            json!({
                                "name": tool,
                                "description": "Echo which server and tool handled the call",
                                "inputSchema": { "type": "object" },
                            })
                        })
                        .collect();
                    json!({ "tools": tools })
                }
                "tools/call" => match request["params"]["name"].as_str().unwrap_or_default() {
                    "add_tool" => {
                        if !tool_names.iter().any(|tool| tool == "late_tool") {
                            tool_names.push("late_tool".to_string());
                        }
                        send(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": { "content": [{ "type": "text", "text": "added" }] },
                            }),
                        );
                        send(
                            &mut stdout,
                            &json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }),
                        );
                        continue;
                    }
                    "structured" => json!({
                        "content": [{ "type": "text", "text": "{\"answer\": 42}" }],
                        "structuredContent": { "answer": 42 },
                    }),
                    "image" => json!({
                        "content": [
                            { "type": "text", "text": "chart" },
                            { "type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png" },
                        ],
                    }),
                    tool => json!({
                        "content": [{ "type": "text", "text": format!("server={name} tool={tool}") }],
                    }),
                },
                "ping" => json!({}),
                _ => {
                    send(
                        &mut stdout,
                        &json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": { "code": -32601, "message": "method not found" },
                        }),
                    );
                    continue;
                }
            };
            send(&mut stdout, &json!({ "jsonrpc": "2.0", "id": id, "result": result }));
        }
    }
}
