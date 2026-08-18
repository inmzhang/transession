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

fn is_semver(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    parts.len() >= 3
        && parts
            .iter()
            .take(3)
            .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
}

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
fn detects_and_imports_current_codex_fixture() {
    let path = fixture("codex_current_sample.jsonl");
    let format = detect_format(&path).unwrap();
    assert_eq!(format, SessionFormat::Codex);

    let session = load_session(&path, SourceFormat::Auto).unwrap();
    assert_eq!(
        session.metadata.session_id,
        "019d5294-7fd5-7e21-bcca-32362218c185"
    );
    assert_eq!(session.metadata.model.as_deref(), Some("gpt-5.6"));
    assert_eq!(
        session
            .metadata
            .extra
            .get("codex_model_provider")
            .and_then(|value| value.as_str()),
        Some("openai")
    );
    assert_eq!(
        session.metadata.platform_version.as_deref(),
        Some("0.147.0")
    );
    assert_eq!(session.metadata.git_branch.as_deref(), Some("main"));
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
    assert!(
        session
            .events
            .iter()
            .any(|event| matches!(event, SessionEvent::ToolResult(_)))
    );
    assert!(session.events.iter().any(|event| {
        matches!(event, SessionEvent::ToolCall(call) if call.name == "exec" && call.arguments.is_string())
    }));
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
fn detects_and_imports_current_claude_fixture() {
    let path = fixture("claude_current_sample.jsonl");
    let format = detect_format(&path).unwrap();
    assert_eq!(format, SessionFormat::Claude);

    let session = load_session(&path, SourceFormat::Auto).unwrap();
    assert_eq!(
        session.metadata.session_id,
        "63679569-7045-45ba-bfef-cad8b1045769"
    );
    assert_eq!(
        session.metadata.platform_version.as_deref(),
        Some("2.1.234")
    );
    assert_eq!(session.metadata.model.as_deref(), Some("claude-opus-4.8"));
    assert!(
        session
            .events
            .iter()
            .any(|event| matches!(event, SessionEvent::Reasoning(_)))
    );
    let message_count = session
        .events
        .iter()
        .filter(|event| matches!(event, SessionEvent::Message(_)))
        .count();
    assert_eq!(message_count, 3);
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
    assert!(!session.events.iter().any(|event| {
        matches!(event, SessionEvent::Message(message) if message.blocks.iter().any(|block| block.text.as_deref().is_some_and(|text| text.contains("Internal command output"))))
    }));
}

#[test]
fn materializes_canonical_codex_layout() {
    let session = load_session(
        &fixture("claude_current_sample.jsonl"),
        SourceFormat::Claude,
    )
    .unwrap();
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
    assert_eq!(first_user_message, "Inspect README.md");

    let text = fs::read_to_string(path).unwrap();
    assert!(text.contains("\"type\":\"input_image\""));
    assert!(text.contains("\"name\":\"Read\""));
    let session_meta =
        serde_json::from_str::<serde_json::Value>(text.lines().next().unwrap()).unwrap();
    assert_eq!(
        session_meta
            .get("payload")
            .and_then(|payload| payload.get("git"))
            .and_then(|git| git.get("branch"))
            .and_then(|value| value.as_str()),
        Some("main")
    );
    let cli_version = serde_json::from_str::<serde_json::Value>(text.lines().next().unwrap())
        .unwrap()
        .get("payload")
        .and_then(|payload| payload.get("cli_version"))
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .unwrap();
    assert!(
        is_semver(&cli_version),
        "unexpected cli_version {cli_version}"
    );
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
                memory_mode TEXT NOT NULL DEFAULT 'enabled',
                thread_source TEXT,
                preview TEXT NOT NULL DEFAULT '',
                history_mode TEXT NOT NULL DEFAULT 'legacy'
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
    assert_eq!(type_counts.get("turn_context"), None);
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
        Some("openai")
    );
    assert!(
        session_meta
            .get("payload")
            .and_then(|value| value.get("cli_version"))
            .and_then(|value| value.as_str())
            .is_some_and(is_semver)
    );
    assert_eq!(
        session_meta
            .get("payload")
            .and_then(|value| value.get("history_mode"))
            .and_then(|value| value.as_str()),
        Some("legacy")
    );
    assert!(
        session_meta
            .get("payload")
            .and_then(|value| value.get("base_instructions"))
            .is_none()
    );

    let (preview, thread_source, history_mode): (String, String, String) = connection
        .query_row(
            "SELECT preview, thread_source, history_mode FROM threads LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(preview, "First prompt");
    assert_eq!(thread_source, "user");
    assert_eq!(history_mode, "legacy");

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
    let session =
        load_session(&fixture("codex_current_sample.jsonl"), SourceFormat::Codex).unwrap();
    let temp = tempdir().unwrap();
    let path = materialize(&session, SessionFormat::Claude, temp.path()).unwrap();

    assert!(path.exists());
    assert!(path.to_string_lossy().contains("/projects/"));
    let history = temp.path().join("history.jsonl");
    assert!(history.exists());
    let text = fs::read_to_string(path).unwrap();
    let mut saw_image = false;
    let mut saw_freeform_tool = false;
    let mut saw_structured_tool_result = false;
    for line in text.lines() {
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(
            value
                .get("version")
                .and_then(|value| value.as_str())
                .is_some_and(is_semver)
        );
        assert_eq!(
            value.get("entrypoint").and_then(|value| value.as_str()),
            Some("cli")
        );
        if let Some(message) = value.get("message") {
            assert!(message.get("content").unwrap().is_array());
            if value.get("type").and_then(|value| value.as_str()) == Some("assistant") {
                assert!(message.get("model").is_none());
            }
            for block in message
                .get("content")
                .and_then(|value| value.as_array())
                .unwrap()
            {
                assert!(!matches!(
                    block.get("type").and_then(|value| value.as_str()),
                    Some("input_text" | "output_text")
                ));
                match block.get("type").and_then(|value| value.as_str()) {
                    Some("image") => saw_image = true,
                    Some("tool_use")
                        if block.get("name").and_then(|value| value.as_str()) == Some("exec") =>
                    {
                        assert!(block.get("input").is_some_and(|value| value.is_object()));
                        saw_freeform_tool = true;
                    }
                    Some("tool_result") => {
                        let content = block.get("content").unwrap();
                        if let Some(items) = content.as_array() {
                            assert!(items.iter().all(|item| {
                                matches!(
                                    item.get("type").and_then(|value| value.as_str()),
                                    Some("text" | "image" | "document")
                                )
                            }));
                            saw_structured_tool_result = true;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    assert!(saw_image);
    assert!(saw_freeform_tool);
    assert!(saw_structured_tool_result);
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
fn resolves_claude_session_ids_from_claude_config_dir_root() {
    let session = load_session(&fixture("claude_sample.jsonl"), SourceFormat::Claude).unwrap();
    let temp = tempdir().unwrap();
    materialize(&session, SessionFormat::Claude, temp.path()).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_transession"))
        .arg("inspect")
        .arg("d89e26cd-11f2-47e8-bea5-a73ad5458483")
        .arg("--from")
        .arg("claude")
        .arg("--json")
        .env_remove("TRANSESSION_CLAUDE_HOME")
        .env_remove("CLAUDE_HOME")
        .env("CLAUDE_CONFIG_DIR", temp.path())
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
fn quick_cli_opens_claude_target_by_default() {
    let mut source_session =
        load_session(&fixture("codex_sample.jsonl"), SourceFormat::Codex).unwrap();
    let source_home = tempdir().unwrap();
    let target_home = tempdir().unwrap();
    source_session.metadata.cwd = Some(target_home.path().join("missing-session-cwd"));
    materialize(&source_session, SessionFormat::Codex, source_home.path()).unwrap();

    let log_path = target_home.path().join("launcher.log");
    let script_path = target_home.path().join("fake-claude.sh");
    fs::write(
        &script_path,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"{}\"\nprintf 'CLAUDE_CONFIG_DIR=%s\\n' \"$CLAUDE_CONFIG_DIR\" >> \"{}\"\nprintf 'CLAUDE_HOME=%s\\n' \"$CLAUDE_HOME\" >> \"{}\"\n",
            log_path.display(),
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
        .arg("codex")
        .arg("--to")
        .arg("claude")
        .arg("--keep-session-id")
        .arg("019cd6bd-10df-7e61-8506-e9ac5bdf4e6e")
        .arg("--output")
        .arg(target_home.path())
        .env("TRANSESSION_CODEX_HOME", source_home.path())
        .env("TRANSESSION_CLAUDE_BIN", &script_path)
        .output()
        .unwrap();

    assert!(output.status.success());
    let log = fs::read_to_string(log_path).unwrap();
    assert!(log.contains("-r"));
    assert!(log.contains("019cd6bd-10df-7e61-8506-e9ac5bdf4e6e"));
    assert!(log.contains(&format!(
        "CLAUDE_CONFIG_DIR={}",
        target_home.path().display()
    )));
    assert!(log.contains(&format!("CLAUDE_HOME={}", target_home.path().display())));
}

#[test]
fn quick_cli_opens_claude_in_installed_home_without_config_override() {
    let mut source_session =
        load_session(&fixture("codex_sample.jsonl"), SourceFormat::Codex).unwrap();
    let source_home = tempdir().unwrap();
    let claude_home = tempdir().unwrap();
    source_session.metadata.cwd = Some(claude_home.path().join("missing-session-cwd"));
    materialize(&source_session, SessionFormat::Codex, source_home.path()).unwrap();

    let log_path = claude_home.path().join("launcher.log");
    let script_path = claude_home.path().join("fake-claude.sh");
    fs::write(
        &script_path,
        format!(
            "#!/bin/sh\nprintf 'CLAUDE_CONFIG_DIR=%s\\n' \"$CLAUDE_CONFIG_DIR\" > \"{}\"\nprintf 'CLAUDE_HOME=%s\\n' \"$CLAUDE_HOME\" >> \"{}\"\n",
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
        .arg("codex")
        .arg("--to")
        .arg("claude")
        .arg("--keep-session-id")
        .arg("019cd6bd-10df-7e61-8506-e9ac5bdf4e6e")
        .arg("--output")
        .arg(claude_home.path())
        .env("TRANSESSION_CODEX_HOME", source_home.path())
        .env("TRANSESSION_CLAUDE_HOME", claude_home.path())
        .env("TRANSESSION_CLAUDE_BIN", &script_path)
        .output()
        .unwrap();

    assert!(output.status.success());
    // Claude Code keeps its account record in `<CLAUDE_CONFIG_DIR>/.claude.json`,
    // so overriding the variable with the store we just wrote into would send
    // the user through login again.
    let log = fs::read_to_string(log_path).unwrap();
    assert_eq!(log, "CLAUDE_CONFIG_DIR=\nCLAUDE_HOME=\n");
}

#[test]
fn quick_cli_opens_claude_target_bootstraps_login_state() {
    let mut source_session =
        load_session(&fixture("codex_sample.jsonl"), SourceFormat::Codex).unwrap();
    let source_home = tempdir().unwrap();
    let target_home = tempdir().unwrap();
    let installed_home = tempdir().unwrap();
    source_session.metadata.cwd = Some(target_home.path().join("missing-session-cwd"));
    materialize(&source_session, SessionFormat::Codex, source_home.path()).unwrap();
    fs::write(
        installed_home.path().join(".credentials.json"),
        "{\"claudeAiOauth\":{}}",
    )
    .unwrap();
    fs::write(
        installed_home.path().join(".claude.json"),
        "{\"oauthAccount\":{}}",
    )
    .unwrap();

    let log_path = target_home.path().join("launcher.log");
    let script_path = target_home.path().join("fake-claude.sh");
    fs::write(
        &script_path,
        format!(
            "#!/bin/sh\nfor file in .credentials.json .claude.json; do\n  if [ ! -e \"$CLAUDE_CONFIG_DIR/$file\" ]; then\n    echo \"missing $file\" >&2\n    exit 1\n  fi\ndone\nprintf '%s\\n' \"$@\" > \"{}\"\n",
            log_path.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&script_path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_transession"))
        .arg("--from")
        .arg("codex")
        .arg("--to")
        .arg("claude")
        .arg("--keep-session-id")
        .arg("019cd6bd-10df-7e61-8506-e9ac5bdf4e6e")
        .arg("--output")
        .arg(target_home.path())
        .env("TRANSESSION_CODEX_HOME", source_home.path())
        .env("TRANSESSION_CLAUDE_HOME", installed_home.path())
        .env("TRANSESSION_CLAUDE_BIN", &script_path)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    let log = fs::read_to_string(log_path).unwrap();
    assert!(log.contains("019cd6bd-10df-7e61-8506-e9ac5bdf4e6e"));
}

#[test]
fn quick_cli_opens_codex_target_by_default_bootstraps_auth() {
    let mut source_session =
        load_session(&fixture("claude_sample.jsonl"), SourceFormat::Claude).unwrap();
    let source_home = tempdir().unwrap();
    let target_home = tempdir().unwrap();
    let installed_home = tempdir().unwrap();
    source_session.metadata.cwd = Some(target_home.path().join("missing-session-cwd"));
    materialize(&source_session, SessionFormat::Claude, source_home.path()).unwrap();
    fs::write(
        installed_home.path().join("auth.json"),
        "{\"access_token\":\"test\"}",
    )
    .unwrap();

    let log_path = target_home.path().join("launcher.log");
    let script_path = target_home.path().join("fake-codex.sh");
    fs::write(
        &script_path,
        format!(
            "#!/bin/sh\nif [ ! -e \"$CODEX_HOME/auth.json\" ]; then\n  echo 'missing auth' >&2\n  exit 1\nfi\nprintf '%s\\n' \"$@\" > \"{}\"\nprintf 'CODEX_HOME=%s\\n' \"$CODEX_HOME\" >> \"{}\"\n",
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
        .arg("--keep-session-id")
        .arg("d89e26cd-11f2-47e8-bea5-a73ad5458483")
        .arg("--output")
        .arg(target_home.path())
        .env("TRANSESSION_CLAUDE_HOME", source_home.path())
        .env("CODEX_HOME", installed_home.path())
        .env("TRANSESSION_CODEX_BIN", &script_path)
        .output()
        .unwrap();

    assert!(output.status.success());
    let log = fs::read_to_string(log_path).unwrap();
    assert!(log.contains("resume"));
    assert!(log.contains("d89e26cd-11f2-47e8-bea5-a73ad5458483"));
}

#[test]
fn quick_cli_opens_target_agent_by_default() {
    let mut source_session =
        load_session(&fixture("claude_sample.jsonl"), SourceFormat::Claude).unwrap();
    let source_home = tempdir().unwrap();
    let target_home = tempdir().unwrap();
    source_session.metadata.cwd = Some(target_home.path().join("missing-session-cwd"));
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
