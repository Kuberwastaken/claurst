use super::*;
use claurst_core::config::{Config, ProviderConfig};
use claurst_core::effort::EffortLevel;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// Exercise config round-trip, model resolution, the real query loop, provider
// factory and HTTP adapter together. All requests go to a one-shot loopback
// server; no API key or live inference service is needed.
async fn capture_minimax_query(model: Option<&str>, effort: Option<&str>) -> Value {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let capture = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut raw = Vec::new();
        let mut buffer = [0_u8; 4096];
        let body_offset = loop {
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0, "request ended before headers");
            raw.extend_from_slice(&buffer[..count]);
            if let Some(end) = raw.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = String::from_utf8(raw[..body_offset].to_vec()).unwrap();
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        while raw.len() < body_offset + length {
            let count = socket.read(&mut buffer).await.unwrap();
            assert!(count > 0, "request ended before body");
            raw.extend_from_slice(&buffer[..count]);
        }
        assert_eq!(
            headers.lines().next(),
            Some("POST /anthropic/v1/messages HTTP/1.1")
        );
        assert!(headers
            .to_ascii_lowercase()
            .contains("x-api-key: test-minimax-key"));
        let body: Value = serde_json::from_slice(&raw[body_offset..body_offset + length]).unwrap();
        let events = [
            json!({"type":"message_start","message":{"id":"msg_test","type":"message","role":"assistant","model":"MiniMax-M3.1-Flash","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":10,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"fresh reasoning"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"fresh-signature"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"thinking","thinking":"second ","signature":"second-"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"thinking_delta","thinking":"reasoning"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"signature_delta","signature":"signature"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"text_delta","text":"done"}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":3}}),
            json!({"type":"message_stop"}),
        ];
        let response: String = events
            .iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {}\n\n",
                    event["type"].as_str().unwrap(),
                    event
                )
            })
            .collect();
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).as_bytes()).await.unwrap();
        body
    });

    let mut saved = Config {
        provider: Some("minimax".to_string()),
        model: model.map(str::to_string),
        effort: effort.map(str::to_string),
        ..Default::default()
    };
    saved.provider_configs.insert(
        "minimax".to_string(),
        ProviderConfig {
            api_key: Some("test-minimax-key".to_string()),
            api_base: Some(format!("http://{address}/anthropic")),
            options: [("service_tier".to_string(), json!("priority"))].into(),
            ..Default::default()
        },
    );
    let saved: Config = serde_json::from_value(serde_json::to_value(saved).unwrap()).unwrap();
    let catalog = Arc::new(claurst_api::ModelRegistry::new());
    let selected = claurst_api::effective_model_for_config(&saved, &catalog);
    if let Some(model) = model {
        assert_eq!(selected, model, "saved/custom IDs must remain exact");
    } else {
        assert_eq!(
            selected,
            catalog.best_model_for_provider("minimax").unwrap()
        );
    }
    let provider = claurst_api::registry::provider_from_config(&saved, "minimax").unwrap();
    let mut providers = claurst_api::ProviderRegistry::new();
    providers.register(provider);
    let mut config = QueryConfig::from_config_with_registry(&saved, &catalog);
    config.provider_registry = Some(Arc::new(providers));
    config.model_registry = Some(catalog);
    config.auto_compact = false;
    config.max_turns = 1;
    config.retry = claurst_api::RetryConfig::with_max_retries(Some(0));
    let mut ctx = deny_all_context();
    ctx.config = saved;
    let client = claurst_api::AnthropicClient::new(claurst_api::client::ClientConfig {
        api_key: "unused-test-key".to_string(),
        ..Default::default()
    })
    .unwrap();
    let mut messages: Vec<Message> = serde_json::from_value(json!([
        {"role":"user","content":[{"type":"text","text":"inspect this"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"aW1hZ2U="}}]},
        {"role":"assistant","content":[{"type":"thinking","thinking":"original reasoning","signature":"original-signature"},{"type":"text","text":"checking"},{"type":"tool_use","id":"call_1","name":"noop_tool","input":{}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"previous tool result"}]}
    ])).unwrap();
    let original_history = serde_json::to_value(&messages).unwrap();
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        run_query_loop(
            &client,
            &mut messages,
            &noop_tools(),
            &ctx,
            &config,
            claurst_core::cost::CostTracker::new(),
            None,
            tokio_util::sync::CancellationToken::new(),
            None,
        ),
    )
    .await
    .expect("query must not hang");
    assert!(
        matches!(outcome, QueryOutcome::EndTurn { .. }),
        "{outcome:?}"
    );
    let body = tokio::time::timeout(std::time::Duration::from_secs(10), capture)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(body["model"], selected);
    assert_eq!(body["stream"], true);
    assert_eq!(body["tools"][0]["name"], "noop_tool");
    // Normalization may strip empty content, but all nonempty reasoning and
    // tool blocks (including their order and signatures) must remain intact.
    assert_eq!(body["messages"][1], original_history[1]);
    assert_eq!(body["messages"][2], original_history[2]);
    if model != Some("MiniMax-M2.7") {
        assert_eq!(body["messages"][0], original_history[0]);
    } else {
        assert_eq!(
            body["messages"][0]["content"][1]["text"],
            "[Image not supported by this model]"
        );
    }
    let reply = serde_json::to_value(messages.last().unwrap()).unwrap();
    assert_eq!(reply["content"][0]["thinking"], "fresh reasoning");
    assert_eq!(reply["content"][0]["signature"], "fresh-signature");
    assert_eq!(reply["content"][1]["thinking"], "second reasoning");
    assert_eq!(reply["content"][1]["signature"], "second-signature");
    body
}

#[tokio::test]
async fn minimax_flash_efforts_reach_http_from_saved_config() {
    for (effort, expected) in [
        (None, None),
        (Some("low"), Some("low")),
        (Some("medium"), Some("medium")),
        (Some("high"), Some("high")),
        (Some("xhigh"), Some("xhigh")),
        (Some("max"), Some("max")),
        (Some("none"), Some("low")),
        (Some("minimal"), Some("low")),
        (Some("ultracode"), Some("max")),
    ] {
        let body = capture_minimax_query(Some("MiniMax-M3.1-Flash"), effort).await;
        assert_eq!(body["thinking"], json!({"type":"adaptive"}), "{effort:?}");
        assert!(body.get("service_tier").is_none());
        match expected {
            Some(expected) => assert_eq!(body["output_config"], json!({"effort":expected})),
            None => assert!(body.get("output_config").is_none()),
        }
    }
}

#[tokio::test]
async fn minimax_legacy_custom_and_catalog_default_http_are_unchanged() {
    for model in [
        Some("MiniMax-M3"),
        Some("MiniMax-M2.7"),
        Some("custom-model"),
        Some("MiniMax-M3.1-Flash-custom"),
    ] {
        for effort in [None, Some("high")] {
            let body = capture_minimax_query(model, effort).await;
            assert_eq!(body["service_tier"], "priority");
            assert!(body.get("output_config").is_none());
            if body["model"] == "MiniMax-M3" && effort.is_some() {
                assert_eq!(body["thinking"], json!({"type":"adaptive"}));
            } else {
                assert!(body.get("thinking").is_none());
            }
        }
    }
}

#[test]
fn minimax_flash_effort_picker_does_not_change_catalog_visibility() {
    use EffortLevel::*;
    let catalog = claurst_api::ModelRegistry::new();
    for registry in [Some(&catalog), Option::None] {
        assert_eq!(
            claurst_api::variant_ladder("minimax", "MiniMax-M3.1-Flash", registry),
            vec![Low, Medium, High, XHigh, Max]
        );
        assert_eq!(
            claurst_api::supported_efforts("minimax", "MiniMax-M3.1-Flash", registry),
            vec![Low, Medium, High, XHigh, Max, Ultracode]
        );
    }
    assert!(
        claurst_api::variant_ladder("minimax", "MiniMax-M3.1-Flash-custom", Option::None)
            .is_empty()
    );
    assert!(claurst_api::variant_ladder("custom", "MiniMax-M3.1-Flash", Option::None).is_empty());
    assert!(catalog.list_by_provider("minimax").len() >= 5);
    let mut visible: Vec<_> = catalog
        .list_visible_by_provider("minimax")
        .into_iter()
        .map(|entry| entry.info.id.to_string())
        .collect();
    visible.sort();
    assert_eq!(visible, vec!["MiniMax-M3", "MiniMax-M3.1-Flash"]);
}

#[tokio::test]
async fn minimax_catalog_default_reaches_http_as_flash() {
    let body = capture_minimax_query(None, None).await;
    assert_eq!(body["model"], "MiniMax-M3.1-Flash");
    assert_eq!(body["thinking"], json!({"type":"adaptive"}));
    assert!(body.get("output_config").is_none());
    assert!(body.get("service_tier").is_none());
}
