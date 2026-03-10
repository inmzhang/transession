use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use rusqlite::Connection;
use tempfile::tempdir;
use transession::formats::{detect_format, load_session, materialize};
use transession::ir::{
    ContentBlock, MessageEvent, ReasoningEvent, SessionEvent, SessionFormat, SourceFormat,
    UniversalSession,
};

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn detects_and_imports_codex_fixture() {
    let path = fixture("codex_sample.jsonl");
    let format = detect_format(&path).unwrap();
    assert_eq!(format, SessionFormat::Codex);

    let session = load_session(&path, SourceFormat::Auto).unwrap();
    assert_eq!(
        session.metadata.session_id,
        "019cd6bd-10df-7e61-8506-e9ac5bdf4e6e"
    );
    assert!(
        session
            .events
            .iter()
            .any(|event| matches!(event, SessionEvent::ToolCall(_)))
    );
    assert!(
        session
            .events
            .iter()
            .any(|event| matches!(event, SessionEvent::ToolResult(_)))
    );
}

#[test]
fn detects_and_imports_claude_fixture() {
    let path = fixture("claude_sample.jsonl");
    let format = detect_format(&path).unwrap();
    assert_eq!(format, SessionFormat::Claude);

    let session = load_session(&path, SourceFormat::Auto).unwrap();
    assert_eq!(
        session.metadata.session_id,
        "d89e26cd-11f2-47e8-bea5-a73ad5458483"
    );
    assert!(
        session
            .events
            .iter()
            .any(|event| matches!(event, SessionEvent::Reasoning(_)))
    );
    assert!(
        session
            .events
            .iter()
            .any(|event| matches!(event, SessionEvent::ToolCall(_)))
    );
    assert!(matches!(session.events[1], SessionEvent::Reasoning(_)));
    assert!(matches!(session.events[2], SessionEvent::Message(_)));
}

#[test]
fn materializes_canonical_codex_layout() {
    let session = load_session(&fixture("claude_sample.jsonl"), SourceFormat::Claude).unwrap();
    let temp = tempdir().unwrap();
    let sqlite = temp.path().join("state_5.sqlite");
    let connection = Connection::open(&sqlite).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                source TEXT NOT NULL,
                model_provider TEXT NOT NULL,
                cwd TEXT NOT NULL,
                title TEXT NOT NULL,
                sandbox_policy TEXT NOT NULL,
                approval_mode TEXT NOT NULL,
                tokens_used INTEGER NOT NULL DEFAULT 0,
                has_user_event INTEGER NOT NULL DEFAULT 0,
                archived INTEGER NOT NULL DEFAULT 0,
                archived_at INTEGER,
                git_sha TEXT,
                git_branch TEXT,
                git_origin_url TEXT,
                cli_version TEXT NOT NULL DEFAULT '',
                first_user_message TEXT NOT NULL DEFAULT '',
                agent_nickname TEXT,
                agent_role TEXT,
                memory_mode TEXT NOT NULL DEFAULT 'enabled'
            );",
        )
        .unwrap();
    let path = materialize(&session, SessionFormat::Codex, temp.path()).unwrap();

    assert!(path.exists());
    assert!(path.to_string_lossy().contains("/sessions/"));

    let index = temp.path().join("session_index.jsonl");
    assert!(index.exists());
    let registered_count: i64 = connection
        .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
        .unwrap();
    assert_eq!(registered_count, 1);
    let (id, title, first_user_message): (String, String, String) = connection
        .query_row(
            "SELECT id, title, first_user_message FROM threads LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(id, session.metadata.session_id);
    assert_eq!(title, id);
    assert!(first_user_message.contains("continuous-codex.sh"));
}

#[test]
fn materialized_codex_sessions_include_turn_events() {
    let temp = tempdir().unwrap();
    let sqlite = temp.path().join("state_5.sqlite");
    let connection = Connection::open(&sqlite).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                source TEXT NOT NULL,
                model_provider TEXT NOT NULL,
                cwd TEXT NOT NULL,
                title TEXT NOT NULL,
                sandbox_policy TEXT NOT NULL,
                approval_mode TEXT NOT NULL,
                tokens_used INTEGER NOT NULL DEFAULT 0,
                has_user_event INTEGER NOT NULL DEFAULT 0,
                archived INTEGER NOT NULL DEFAULT 0,
                archived_at INTEGER,
                git_sha TEXT,
                git_branch TEXT,
                git_origin_url TEXT,
                cli_version TEXT NOT NULL DEFAULT '',
                first_user_message TEXT NOT NULL DEFAULT '',
                agent_nickname TEXT,
                agent_role TEXT,
                memory_mode TEXT NOT NULL DEFAULT 'enabled'
            );",
        )
        .unwrap();

    let mut session = UniversalSession::new("turn-events".to_string());
    session.events.push(SessionEvent::Message(MessageEvent {
        id: None,
        parent_id: None,
        role: "developer".to_string(),
        timestamp: None,
        blocks: vec![ContentBlock::text(
            "input_text",
            "Repository instructions apply.",
        )],
        metadata: Default::default(),
    }));
    session.events.push(SessionEvent::Message(MessageEvent {
        id: None,
        parent_id: None,
        role: "user".to_string(),
        timestamp: None,
        blocks: vec![ContentBlock::text("input_text", "First prompt")],
        metadata: Default::default(),
    }));
    session.events.push(SessionEvent::Reasoning(ReasoningEvent {
        id: None,
        parent_id: None,
        timestamp: None,
        summary: vec!["Thinking through the task.".to_string()],
        metadata: Default::default(),
    }));
    session.events.push(SessionEvent::Message(MessageEvent {
        id: None,
        parent_id: None,
        role: "assistant".to_string(),
        timestamp: None,
        blocks: vec![ContentBlock::text(
            "output_text",
            "First answer with context.",
        )],
        metadata: Default::default(),
    }));
    session.events.push(SessionEvent::Message(MessageEvent {
        id: None,
        parent_id: None,
        role: "user".to_string(),
        timestamp: None,
        blocks: vec![ContentBlock::text("input_text", "Second prompt")],
        metadata: Default::default(),
    }));
    session.events.push(SessionEvent::Message(MessageEvent {
        id: None,
        parent_id: None,
        role: "assistant".to_string(),
        timestamp: None,
        blocks: vec![ContentBlock::text("output_text", "Second answer.")],
        metadata: Default::default(),
    }));

    let path = materialize(&session, SessionFormat::Codex, temp.path()).unwrap();
    let lines = fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();

    let type_counts = lines
        .iter()
        .filter_map(|value| value.get("type").and_then(|value| value.as_str()))
        .fold(
            std::collections::BTreeMap::<String, usize>::new(),
            |mut acc, value| {
                *acc.entry(value.to_string()).or_insert(0) += 1;
                acc
            },
        );
    assert_eq!(type_counts.get("session_meta"), Some(&1));
    assert_eq!(type_counts.get("turn_context"), Some(&2));
    assert_eq!(type_counts.get("event_msg"), Some(&9));

    let session_meta = lines
        .iter()
        .find(|value| value.get("type").and_then(|value| value.as_str()) == Some("session_meta"))
        .unwrap();
    assert_eq!(
        session_meta
            .get("payload")
            .and_then(|value| value.get("model_provider"))
            .and_then(|value| value.as_str()),
        Some("imported")
    );

    let turn_context = lines
        .iter()
        .find(|value| value.get("type").and_then(|value| value.as_str()) == Some("turn_context"))
        .unwrap();
    let turn_payload = turn_context.get("payload").unwrap();
    assert!(turn_payload.get("model").is_none());
    assert_eq!(
        turn_payload.get("collaboration_mode"),
        Some(&serde_json::json!({ "mode": "default" }))
    );

    let event_types = lines
        .iter()
        .filter(|value| value.get("type").and_then(|value| value.as_str()) == Some("event_msg"))
        .filter_map(|value| {
            value
                .get("payload")
                .and_then(|value| value.get("type"))
                .and_then(|value| value.as_str())
        })
        .fold(
            std::collections::BTreeMap::<String, usize>::new(),
            |mut acc, value| {
                *acc.entry(value.to_string()).or_insert(0) += 1;
                acc
            },
        );
    assert_eq!(event_types.get("task_started"), Some(&2));
    assert_eq!(event_types.get("user_message"), Some(&2));
    assert_eq!(event_types.get("agent_reasoning"), Some(&1));
    assert_eq!(event_types.get("agent_message"), Some(&2));
    assert_eq!(event_types.get("task_complete"), Some(&2));
}

#[test]
fn materializes_canonical_claude_layout() {
    let session = load_session(&fixture("codex_sample.jsonl"), SourceFormat::Codex).unwrap();
    let temp = tempdir().unwrap();
    let path = materialize(&session, SessionFormat::Claude, temp.path()).unwrap();

    assert!(path.exists());
    assert!(path.to_string_lossy().contains("/projects/"));
    let history = temp.path().join("history.jsonl");
    assert!(history.exists());
    let text = fs::read_to_string(path).unwrap();
    assert!(!text.contains("\"type\":\"input_text\""));
    assert!(!text.contains("\"type\":\"output_text\""));
    for line in text.lines() {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        if let Some(message) = value.get("message") {
            assert!(message.get("content").unwrap().is_array());
            if value.get("type").and_then(|value| value.as_str()) == Some("assistant") {
                assert!(message.get("model").is_none());
            }
        }
    }
}

#[test]
fn writes_ir_json() {
    let session = load_session(&fixture("claude_sample.jsonl"), SourceFormat::Claude).unwrap();
    let temp = tempdir().unwrap();
    let output = temp.path().join("session.json");
    let path = materialize(&session, SessionFormat::Ir, &output).unwrap();
    let text = fs::read_to_string(path).unwrap();
    assert!(text.contains("\"ir_version\": \"transession/v1\""));
}

#[test]
fn auto_detects_pretty_printed_ir() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("session.json");
    fs::write(
        &input,
        r#"{
  "ir_version": "transession/v1",
  "metadata": {
    "session_id": "test-session"
  },
  "events": []
}"#,
    )
    .unwrap();

    let format = detect_format(&input).unwrap();
    assert_eq!(format, SessionFormat::Ir);
}

#[test]
fn projects_codex_developer_messages_into_claude() {
    let mut session = UniversalSession::new("developer-projection".to_string());
    session.events.push(SessionEvent::Message(MessageEvent {
        id: None,
        parent_id: None,
        role: "developer".to_string(),
        timestamp: None,
        blocks: vec![ContentBlock::text(
            "input_text",
            "Follow the project instructions carefully.",
        )],
        metadata: Default::default(),
    }));

    let temp = tempdir().unwrap();
    let path = materialize(&session, SessionFormat::Claude, temp.path()).unwrap();
    let text = fs::read_to_string(path).unwrap();
    assert!(text.contains("[transession imported developer message]"));
}

#[test]
fn resolves_codex_session_ids_from_default_store_roots() {
    let session = load_session(&fixture("codex_sample.jsonl"), SourceFormat::Codex).unwrap();
    let temp = tempdir().unwrap();
    materialize(&session, SessionFormat::Codex, temp.path()).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_transession"))
        .arg("inspect")
        .arg("019cd6bd-10df-7e61-8506-e9ac5bdf4e6e")
        .arg("--from")
        .arg("codex")
        .arg("--json")
        .env("TRANSESSION_CODEX_HOME", temp.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("\"detected_format\": \"codex\""));
}

#[test]
fn resolves_claude_session_ids_from_default_store_roots() {
    let session = load_session(&fixture("claude_sample.jsonl"), SourceFormat::Claude).unwrap();
    let temp = tempdir().unwrap();
    materialize(&session, SessionFormat::Claude, temp.path()).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_transession"))
        .arg("inspect")
        .arg("d89e26cd-11f2-47e8-bea5-a73ad5458483")
        .arg("--from")
        .arg("claude")
        .arg("--json")
        .env("TRANSESSION_CLAUDE_HOME", temp.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("\"detected_format\": \"claude\""));
}

#[test]
fn quick_cli_converts_by_session_id_and_prints_resume_hint() {
    let source_session =
        load_session(&fixture("claude_sample.jsonl"), SourceFormat::Claude).unwrap();
    let source_home = tempdir().unwrap();
    let target_home = tempdir().unwrap();
    materialize(&source_session, SessionFormat::Claude, source_home.path()).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_transession"))
        .arg("--from")
        .arg("claude")
        .arg("--to")
        .arg("codex")
        .arg("d89e26cd-11f2-47e8-bea5-a73ad5458483")
        .arg("--no-open")
        .env("TRANSESSION_CLAUDE_HOME", source_home.path())
        .env("TRANSESSION_CODEX_HOME", target_home.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("created codex session:"));
    assert!(stdout.contains("resume with: codex resume "));
}

#[test]
fn quick_cli_opens_target_agent_by_default() {
    let source_session =
        load_session(&fixture("claude_sample.jsonl"), SourceFormat::Claude).unwrap();
    let source_home = tempdir().unwrap();
    let target_home = tempdir().unwrap();
    materialize(&source_session, SessionFormat::Claude, source_home.path()).unwrap();

    let log_path = target_home.path().join("launcher.log");
    let script_path = target_home.path().join("fake-codex.sh");
    fs::write(
        &script_path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\nprintf 'CODEX_HOME=%s\\n' \"$CODEX_HOME\" >> \"{}\"\n",
            log_path.display(),
            log_path.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&script_path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_transession"))
        .arg("--from")
        .arg("claude")
        .arg("--to")
        .arg("codex")
        .arg("d89e26cd-11f2-47e8-bea5-a73ad5458483")
        .arg("--output")
        .arg(target_home.path())
        .env("TRANSESSION_CLAUDE_HOME", source_home.path())
        .env("TRANSESSION_CODEX_BIN", &script_path)
        .output()
        .unwrap();

    assert!(output.status.success());
    let log = fs::read_to_string(log_path).unwrap();
    assert!(log.contains("resume"));
    assert!(log.contains("CODEX_HOME="));
}
