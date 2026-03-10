use std::fs;

use tempfile::tempdir;
use transession::formats::{detect_format, load_session, materialize};
use transession::ir::{SessionEvent, SessionFormat, SourceFormat};

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
