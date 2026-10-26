//! The schema-rejection recovery loop, end to end at the api layer.
//!
//! The prevention layer (`schema_dialect_map::filter_tools`) strips what the
//! dialect table already knows. When a provider still rejects a schema, the
//! recovery layer must recognize the 400, learn the named construct, and make
//! the NEXT request drop it, verified here against a one-shot local server
//! playing an OpenAI-compatible provider, without touching the developer's
//! real `~/.claurst/schema-quirks.json`.

use claurst_api::schema_dialect_map::{filter_tools, recover_description};
use claurst_api::{LlmProvider, OpenAiCompatProvider, ProviderError, ProviderRequest};
use claurst_core::types::{Message, ToolDefinition};
use claurst_schema_dialect::quirks;
use serde_json::json;

/// Serve N sequential raw HTTP responses on a loopback port; return the base
/// URL and the collected request bodies. Each response is served after its
/// request (headers + content-length body) has been fully drained.
async fn serve(responses: Vec<String>) -> (String, tokio::sync::mpsc::Receiver<Vec<u8>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move {
        for response in responses {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let n = sock.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let len = text[..end]
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + len {
                        break;
                    }
                }
            }
            tx.send(buf.clone()).await.unwrap();
            sock.write_all(response.as_bytes()).await.unwrap();
            sock.shutdown().await.ok();
        }
    });
    (format!("http://{addr}"), rx)
}

fn request_with_tool(schema: serde_json::Value) -> ProviderRequest {
    ProviderRequest {
        model: "m".to_string(),
        messages: vec![Message::user("hi")],
        system_prompt: None,
        tools: vec![ToolDefinition {
            name: "lookup".to_string(),
            description: "look things up".to_string(),
            input_schema: schema,
        }],
        max_tokens: 16,
        temperature: None,
        top_p: None,
        top_k: None,
        stop_sequences: vec![],
        thinking: None,
        provider_options: json!({}),
    }
}

fn ok_body() -> String {
    let body = r#"{"id":"x","model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#;
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

#[tokio::test]
async fn a_rejected_keyword_is_learned_and_the_next_request_drops_it() {
    // Per-test quirk store so this test neither reads nor writes the
    // developer's real quirks file.
    let dir = tempfile::tempdir().unwrap();
    quirks::use_test_path(dir.path().join("quirks.json"));

    let schema = json!({
        "type": "object",
        "properties": { "items": { "type": "array" } },
        "additionalProperties": false
    });
    let reject = {
        let body = r#"{"error":{"message":"'additionalProperties' is not permitted","type":"invalid_request_error"}}"#;
        format!(
            "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    };
    let (base, mut rx) = serve(vec![reject, ok_body()]).await;
    let provider = OpenAiCompatProvider::new("openai", "T", base);

    // First attempt: the dialect table does not know `uniqueItems` is bad
    // for this provider, so it goes out and comes back as a hard 400.
    let err = provider
        .create_message(request_with_tool(schema.clone()))
        .await
        .expect_err("first attempt must fail with the rejection");
    assert!(
        matches!(
            err,
            ProviderError::Other {
                status: Some(400),
                ..
            } | ProviderError::InvalidRequest { .. }
        ),
        "expected a 400-class rejection, got {err}"
    );
    let first_body = rx.recv().await.unwrap();
    assert!(
        String::from_utf8_lossy(&first_body).contains("additionalProperties"),
        "prevention must not have stripped it yet"
    );

    // Recovery layer recognizes the rejection and learns the keyword.
    let description =
        recover_description("openai", &err.to_string()).expect("rejection must be actionable");
    assert!(
        description.contains("additionalProperties"),
        "learned description should name the keyword: {description}"
    );

    // Second attempt: the same schema is now filtered without the keyword.
    let response = provider
        .create_message(request_with_tool(schema))
        .await
        .expect("re-filtered request must be accepted");
    assert!(!response.content.is_empty());
    let second_body = rx.recv().await.unwrap();
    let text = String::from_utf8_lossy(&second_body);
    assert!(
        !text.contains("additionalProperties"),
        "retry must not resend the rejected keyword"
    );
    assert!(
        text.contains("\"lookup\""),
        "the tool itself must survive the filter"
    );
}

#[tokio::test]
async fn recovery_fires_at_most_once_for_the_same_construct() {
    let dir = tempfile::tempdir().unwrap();
    quirks::use_test_path(dir.path().join("quirks.json"));

    // Prevention would normally strip it; simulate a rejection the table did
    // not cover and confirm the second, identical rejection is NOT retryable.
    let first = recover_description(
        "openai",
        "[openai] Invalid request: 'additionalProperties' is not permitted",
    );
    assert!(first.is_some(), "first rejection should be learned");
    let second = recover_description(
        "openai",
        "[openai] Invalid request: 'additionalProperties' is not permitted",
    );
    assert!(
        second.is_none(),
        "a repeat rejection must not trigger another retry"
    );
}

#[test]
fn filter_and_recover_round_trip_through_the_same_dialect() {
    let dir = tempfile::tempdir().unwrap();
    quirks::use_test_path(dir.path().join("quirks.json"));

    let schema = json!({
        "type": "object",
        "properties": { "when": { "type": "string", "format": "email" } }
    });
    let tools = vec![ToolDefinition {
        name: "fetch".to_string(),
        description: "fetch".to_string(),
        input_schema: schema,
    }];
    // Before learning, `format` stays (it is a supported keyword).
    assert_eq!(
        filter_tools("openai", &tools)[0].input_schema["properties"]["when"]["format"],
        "email"
    );
    // A rejection naming the format is actionable.
    let learned = recover_description(
        "openai",
        "[openai] Invalid request: 'email' is not a valid format",
    )
    .expect("format rejection must be actionable");
    assert!(learned.contains("email"), "{learned}");
    // After learning, the format is stripped from subsequent requests.
    let filtered = filter_tools("openai", &tools);
    assert!(
        filtered[0].input_schema["properties"]["when"]
            .get("format")
            .is_none(),
        "learned format must be stripped on re-filter"
    );
    assert_eq!(
        filtered[0].input_schema["properties"]["when"]["type"], "string",
        "the property itself must survive"
    );
}

/// `CLAURST_SCHEMA_QUIRKS_PATH` must isolate the store even though this test
/// binary compiles claurst-schema-dialect with test-support, where the
/// fail-closed gate would otherwise return None. A dependent's test binary
/// that forgets `use_test_path` has the env var as its only lever; without
/// this, learning a rejection here would poison the developer's real
/// `~/.claurst/schema-quirks.json`.
#[test]
fn env_var_isolation_directs_the_store_in_test_builds() {
    // No use_test_path here: the env var must be honored on its own. Other
    // tests in this binary set their own thread-local override first, so the
    // process-global env var is only visible to this thread's store access.
    struct EnvGuard {
        saved: Option<std::ffi::OsString>,
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.saved.take() {
                Some(v) => std::env::set_var("CLAURST_SCHEMA_QUIRKS_PATH", v),
                None => std::env::remove_var("CLAURST_SCHEMA_QUIRKS_PATH"),
            }
        }
    }
    let _guard = EnvGuard {
        saved: std::env::var_os("CLAURST_SCHEMA_QUIRKS_PATH"),
    };
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("quirks.json");
    std::env::set_var("CLAURST_SCHEMA_QUIRKS_PATH", &store);

    // A rejection naming a keyword the table has not rejected live yet.
    let learned = recover_description(
        "openai",
        "[openai] Invalid request: 'uniqueItems' is not permitted",
    )
    .expect("the rejection must be learned");
    assert!(learned.contains("uniqueItems"), "{learned}");

    let persisted = std::fs::read_to_string(&store).expect("env-directed store must be written");
    assert!(
        persisted.contains("uniqueItems"),
        "the learned keyword must land in the env-directed store, not the real home: {persisted}"
    );
    // Reads must come back from the same store, including after the cache is
    // dropped (simulating a restart).
    quirks::reset_cache_for_tests();
    let tools = vec![ToolDefinition {
        name: "t".to_string(),
        description: "d".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": { "tags": { "type": "array", "uniqueItems": true } }
        }),
    }];
    assert!(
        !filter_tools("openai", &tools)[0].input_schema["properties"]["tags"]
            .get("uniqueItems")
            .is_some(),
        "the learned keyword must be stripped from later requests via the env-directed store"
    );
}
