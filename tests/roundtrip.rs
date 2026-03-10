use std::fs;
use std::process::Command;

use tempfile::tempdir;
use transession::formats::{detect_format, load_session, materialize};
use transession::ir::{
    ContentBlock, MessageEvent, SessionEvent, SessionFormat, SourceFormat, UniversalSession,
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
    let path = materialize(&session, SessionFormat::Codex, temp.path()).unwrap();

    assert!(path.exists());
    assert!(path.to_string_lossy().contains("/sessions/"));

    let index = temp.path().join("session_index.jsonl");
    assert!(index.exists());
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
