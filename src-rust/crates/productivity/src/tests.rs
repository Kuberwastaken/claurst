//! Tests for the scan + aggregate pipeline, run against fixture transcripts
//! in an isolated `CLAURST_HOME` (honored by `Settings::config_dir`).

use crate::model::SessionSummary;
use crate::{render_markdown, report_from_summaries};
use serde_json::json;
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Scan tests (fixture transcripts)
// ---------------------------------------------------------------------------

fn fixture_home() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    // SAFETY: tests run single-threaded per process on this key; other tests
    // in this binary do not read CLAURST_HOME concurrently.
    std::env::set_var("CLAURST_HOME", dir.path());
    dir
}

fn user_msg(text: &str) -> serde_json::Value {
    user_msg_with("u1", "/tmp/proj-a", text)
}

fn user_msg_with(uuid: &str, cwd: &str, text: &str) -> serde_json::Value {
    json!({
        "type": "user",
        "uuid": uuid,
        "parentUuid": null,
        "timestamp": "2026-06-01T10:00:00Z",
        "sessionId": "s1",
        "cwd": cwd,
        "message": { "role": "user", "content": text }
    })
}

fn assistant_msg(tool: Option<(&str, serde_json::Value)>, tokens: u64) -> serde_json::Value {
    let content: Vec<serde_json::Value> = match tool {
        Some((name, input)) => vec![
            json!({ "type": "text", "text": "done" }),
            json!({ "type": "tool_use", "id": "t1", "name": name, "input": input }),
        ],
        None => vec![json!({ "type": "text", "text": "hello" })],
    };
    json!({
        "type": "assistant",
        "uuid": "a1",
        "parentUuid": "u1",
        "timestamp": "2026-06-01T10:01:00Z",
        "sessionId": "s1",
        "cwd": "/tmp/proj-a",
        "message": {
            "role": "assistant",
            "content": content,
            "cost": {
                "input_tokens": tokens,
                "output_tokens": tokens / 2,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 10,
                "cost_usd": 0.01
            }
        },
        "model": "claude-sonnet-4-6"
    })
}

#[test]
fn scan_fixture_pipeline() {
    // Single test on purpose: these cases share CLAURST_HOME and the tests in
    // one binary run on parallel threads, so mutating the env must be
    // sequential.
    let _home = fixture_home();
    let root = std::path::PathBuf::from(std::env::var("CLAURST_HOME").unwrap_or_default());

    // -- parse + cache --
    let lines = vec![
        user_msg("fix the bug"),
        assistant_msg(Some(("file_edit", json!({"path": "x.rs"}))), 1000),
    ];
    crate::scan::write_fixture(&root, "/tmp/proj-a", "session-1", &lines).expect("fixture");

    let scan = crate::scan::scan_all();
    assert_eq!(scan.scanned_files, 1);
    assert_eq!(scan.parse_errors, 0);
    assert_eq!(scan.summaries.len(), 1);

    let s = &scan.summaries[0];
    assert_eq!(s.user_msgs, 1);
    assert_eq!(s.assistant_msgs, 1);
    assert_eq!(s.user_chars, 11);
    assert_eq!(s.assistant_chars, 4);
    // file_edit normalizes to "edit"
    assert_eq!(s.tools.get("edit"), Some(&1));
    assert_eq!(s.total_tool_calls(), 1);
    assert_eq!(s.input_tokens, 1000);
    assert_eq!(s.output_tokens, 500);
    assert_eq!(s.cache_read_tokens, 10);
    assert_eq!(s.project.as_deref(), Some("proj-a"));
    assert_eq!(s.active_dates.len(), 1);
    assert!(!s.hour_hist.iter().all(|v| *v == 0));

    // Second scan must hit the cache: nothing changed.
    let scan2 = crate::scan::scan_all();
    assert_eq!(scan2.cache_hits, 1);
    assert_eq!(scan2.scanned_files, 1);

    // -- tombstones --
    let lines = vec![
        user_msg_with("u-keep", "/tmp/proj-b", "keep me"),
        json!({ "type": "tombstone", "deletedUuid": "u-gone" }),
        user_msg_with("u-gone", "/tmp/proj-b", "deleted me"),
    ];
    crate::scan::write_fixture(&root, "/tmp/proj-b", "session-2", &lines).expect("fixture");

    let scan = crate::scan::scan_all();
    let s = scan
        .summaries
        .iter()
        .find(|s| s.project.as_deref() == Some("proj-b"))
        .expect("proj-b summary");
    assert_eq!(s.user_msgs, 1);
    assert_eq!(s.user_chars, 7);

    // -- unparseable lines are skipped, the file still counts as scanned --
    crate::scan::write_fixture(
        &root,
        "/tmp/proj-c",
        "session-3",
        &[json!("{ this is not valid json")],
    )
    .expect("fixture");

    let scan = crate::scan::scan_all();
    assert_eq!(scan.parse_errors, 0);
    assert_eq!(scan.scanned_files, 3);
    let empty = scan
        .summaries
        .iter()
        .filter(|s| s.user_msgs + s.assistant_msgs == 0)
        .count();
    assert_eq!(empty, 1);
}

// ---------------------------------------------------------------------------
// Aggregate tests (in-memory summaries)
// ---------------------------------------------------------------------------

fn summary(project: &str, user: u32, asst: u32, tools: &[(&str, u32)]) -> SessionSummary {
    let mut t = BTreeMap::new();
    for (k, v) in tools {
        t.insert(k.to_string(), *v);
    }
    SessionSummary {
        project: Some(project.to_string()),
        working_dir: Some(format!("/home/u/{project}")),
        model: Some("claude-sonnet-4-6".to_string()),
        user_msgs: user,
        assistant_msgs: asst,
        user_chars: (user as u64) * 50,
        assistant_chars: (asst as u64) * 200,
        tools: t,
        input_tokens: 1000,
        output_tokens: 500,
        cache_read_tokens: 2000,
        active_dates: vec!["2026-06-01".to_string(), "2026-06-02".to_string()],
        ..SessionSummary::default()
    }
}

#[test]
fn aggregates_basic_totals() {
    let summaries = vec![
        summary("alpha", 3, 3, &[("read", 5), ("edit", 2), ("bash", 4)]),
        summary("alpha", 2, 2, &[("read", 1), ("apply_patch", 3)]),
        summary("beta", 5, 5, &[("agentgrep", 7), ("browser", 1)]),
    ];
    let r = report_from_summaries(summaries);

    assert_eq!(r.total_sessions, 3);
    assert_eq!(r.user_prompts, 10);
    assert_eq!(r.assistant_messages, 10);
    // read 6 + edit 2 + bash 4 + apply_patch 3 + agentgrep 7 + browser 1 = 23
    assert_eq!(r.total_tool_calls, 23);
    // edit 2 + apply_patch 3 = 5
    assert_eq!(r.code_edits, 5);
    assert_eq!(r.commands_run, 4);
    assert_eq!(r.searches, 7);
    assert_eq!(r.web_actions, 1);
    assert_eq!(r.distinct_projects, 2);
    assert!(r.power_score > 0);
    assert!(!r.archetype.is_empty());
}

#[test]
fn top_lists_sorted_desc() {
    let summaries = vec![
        summary("alpha", 1, 1, &[("read", 10)]),
        summary("alpha", 1, 1, &[("read", 5)]),
        summary("gamma", 1, 1, &[("bash", 1)]),
    ];
    let r = report_from_summaries(summaries);
    let top_project = r.top_projects.first().expect("non-empty");
    assert_eq!(top_project.name, "alpha");
    assert_eq!(top_project.count, 2);
    let top_tool = r.top_tools.first().expect("non-empty");
    assert_eq!(top_tool.name, "read");
    assert_eq!(top_tool.count, 15);
}

#[test]
fn streaks_and_active_days_dedup() {
    let mut s = summary("alpha", 1, 1, &[("read", 1)]);
    s.active_dates = vec![
        "2026-05-10".to_string(),
        "2026-05-11".to_string(),
        "2026-05-12".to_string(),
        "2026-05-20".to_string(),
    ];
    let r = report_from_summaries(vec![s]);
    assert_eq!(r.active_days, 4);
    assert_eq!(r.longest_streak, 3);
    assert_eq!(r.first_day.as_deref(), Some("2026-05-10"));
    assert_eq!(r.last_day.as_deref(), Some("2026-05-20"));
}

#[test]
fn renders_markdown() {
    let summaries = vec![summary(
        "alpha",
        4,
        4,
        &[("read", 5), ("edit", 3), ("bash", 2)],
    )];
    let r = report_from_summaries(summaries);

    let md = render_markdown(&r);
    assert!(md.contains("Productivity Report"));
    assert!(md.contains("Power Score"));
    assert!(md.contains("Top projects"));
}

#[test]
fn empty_report_is_safe() {
    let r = report_from_summaries(vec![]);
    assert_eq!(r.total_sessions, 0);
    let md = render_markdown(&r);
    assert!(md.contains("Productivity Report"));
}
