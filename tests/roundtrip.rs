use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::Connection;
use tempfile::{TempDir, tempdir};
use transession::formats::{detect_format, load_session, materialize};
use transession::ir::{
    ContentBlock, MessageEvent, ReasoningEvent, SessionEvent, SessionFormat, UniversalSession,
};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

fn is_semver(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    parts.len() >= 3
        && parts
            .iter()
            .take(3)
            .all(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
}

/// Columns every supported Codex release has. `state_db` appends the newer ones
/// on request so one test still exercises the older-schema fallback.
const THREAD_COLUMNS: &str = "id TEXT PRIMARY KEY,
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
    memory_mode TEXT NOT NULL DEFAULT 'enabled'";

const PREVIEW_COLUMNS: &str = ",
    thread_source TEXT,
    preview TEXT NOT NULL DEFAULT '',
    history_mode TEXT NOT NULL DEFAULT 'legacy'";

fn state_db(root: &Path, extra_columns: &str) -> Connection {
    let connection = Connection::open(root.join("state_5.sqlite")).unwrap();
    connection
        .execute_batch(&format!(
            "CREATE TABLE threads ({THREAD_COLUMNS}{extra_columns});"
        ))
        .unwrap();
    connection
}

/// Stand-in for `codex`/`claude` that records how `transession` invoked it.
fn fake_cli(dir: &TempDir, name: &str, body: &str) -> PathBuf {
    let path = dir.path().join(name);
    fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).unwrap();
    path
}

fn jsonl(path: &Path) -> Vec<serde_json::Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn detects_and_imports_codex_fixture() {
    let path = fixture("codex_sample.jsonl");
    assert_eq!(detect_format(&path).unwrap(), SessionFormat::Codex);

    let session = load_session(&path, None).unwrap();
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
    assert_eq!(detect_format(&path).unwrap(), SessionFormat::Codex);

    let session = load_session(&path, None).unwrap();
    assert_eq!(
        session.metadata.session_id,
        "019d5294-7fd5-7e21-bcca-32362218c185"
    );
    assert_eq!(session.metadata.model.as_deref(), Some("gpt-5.6"));
    assert_eq!(
        session.metadata.extra["codex_model_provider"].as_str(),
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
    // Freeform tools spell their payload as a plain (non-JSON) string.
    assert!(session.events.iter().any(|event| {
        matches!(event, SessionEvent::ToolCall(call) if call.name == "exec" && call.arguments.is_string())
    }));
    // Codex states failure in the output text rather than a flag; the fixture
    // holds one successful result and one `Exit code: 127`.
    let failures: Vec<bool> = session
        .events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::ToolResult(result) => Some(result.is_error),
            _ => None,
        })
        .collect();
    assert_eq!(failures, [false, true]);
}

#[test]
fn detects_and_imports_claude_fixture() {
    let path = fixture("claude_sample.jsonl");
    assert_eq!(detect_format(&path).unwrap(), SessionFormat::Claude);

    let session = load_session(&path, None).unwrap();
    assert_eq!(
        session.metadata.session_id,
        "d89e26cd-11f2-47e8-bea5-a73ad5458483"
    );
    // A single assistant entry splits into reasoning followed by its message.
    assert!(matches!(session.events[1], SessionEvent::Reasoning(_)));
    assert!(matches!(session.events[2], SessionEvent::Message(_)));
    assert!(
        session
            .events
            .iter()
            .any(|event| matches!(event, SessionEvent::ToolCall(_)))
    );
}

#[test]
fn detects_and_imports_current_claude_fixture() {
    let path = fixture("claude_current_sample.jsonl");
    assert_eq!(detect_format(&path).unwrap(), SessionFormat::Claude);

    let session = load_session(&path, None).unwrap();
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
    assert!(
        session
            .events
            .iter()
            .any(|event| matches!(event, SessionEvent::ToolResult(_)))
    );
    assert_eq!(
        session
            .events
            .iter()
            .filter(|event| matches!(event, SessionEvent::Message(_)))
            .count(),
        3
    );
    // `isMeta` entries are injected context, not conversation.
    assert!(!session.events.iter().any(|event| {
        matches!(event, SessionEvent::Message(message) if message.blocks.iter().any(|block| block.text.as_deref().is_some_and(|text| text.contains("Internal command output"))))
    }));
}

#[test]
fn materializes_canonical_codex_layout() {
    let session = load_session(
        &fixture("claude_current_sample.jsonl"),
        Some(SessionFormat::Claude),
    )
    .unwrap();
    let temp = tempdir().unwrap();
    let connection = state_db(temp.path(), "");

    let path = materialize(&session, SessionFormat::Codex, temp.path()).unwrap();
    assert!(path.to_string_lossy().contains("/sessions/"));
    assert!(temp.path().join("session_index.jsonl").exists());

    let (id, title, first_user_message): (String, String, String) = connection
        .query_row(
            "SELECT id, title, first_user_message FROM threads",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(id, session.metadata.session_id);
    assert_eq!(title, "Inspect README.md");
    assert_eq!(first_user_message, "Inspect README.md");

    let lines = jsonl(&path);
    let payload = &lines[0]["payload"];
    assert_eq!(payload["git"]["branch"].as_str(), Some("main"));
    assert!(payload["cli_version"].as_str().is_some_and(is_semver));

    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("\"type\":\"input_image\""));
    assert!(text.contains("\"name\":\"Read\""));
}

#[test]
fn materialized_codex_sessions_include_turn_events() {
    fn message(role: &str, text: &str) -> SessionEvent {
        SessionEvent::Message(MessageEvent {
            id: None,
            parent_id: None,
            role: role.to_string(),
            timestamp: None,
            blocks: vec![ContentBlock::text("input_text", text)],
            metadata: Default::default(),
        })
    }

    let temp = tempdir().unwrap();
    let connection = state_db(temp.path(), PREVIEW_COLUMNS);

    let mut session = UniversalSession::new("turn-events".to_string());
    session
        .events
        .push(message("developer", "Repository instructions apply."));
    session.events.push(message("user", "First prompt"));
    session.events.push(SessionEvent::Reasoning(ReasoningEvent {
        id: None,
        parent_id: None,
        timestamp: None,
        summary: vec!["Thinking through the task.".to_string()],
        metadata: Default::default(),
    }));
    session
        .events
        .push(message("assistant", "First answer with context."));
    session.events.push(message("user", "Second prompt"));
    session.events.push(message("assistant", "Second answer."));

    let lines = jsonl(&materialize(&session, SessionFormat::Codex, temp.path()).unwrap());

    let count = |kind: &str| lines.iter().filter(|line| line["type"] == kind).count();
    assert_eq!(count("session_meta"), 1);
    assert_eq!(count("turn_context"), 0);
    assert_eq!(count("event_msg"), 9);

    let payload = &lines[0]["payload"];
    assert_eq!(payload["model_provider"].as_str(), Some("openai"));
    assert_eq!(payload["history_mode"].as_str(), Some("legacy"));
    assert!(payload["cli_version"].as_str().is_some_and(is_semver));
    assert!(payload.get("base_instructions").is_none());

    let (preview, thread_source, history_mode): (String, String, String) = connection
        .query_row(
            "SELECT preview, thread_source, history_mode FROM threads",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(preview, "First prompt");
    assert_eq!(thread_source, "user");
    assert_eq!(history_mode, "legacy");

    // The developer message joins the first user turn instead of opening one.
    let event_count = |kind: &str| {
        lines
            .iter()
            .filter(|line| line["type"] == "event_msg" && line["payload"]["type"] == kind)
            .count()
    };
    assert_eq!(event_count("task_started"), 2);
    assert_eq!(event_count("user_message"), 2);
    assert_eq!(event_count("agent_reasoning"), 1);
    assert_eq!(event_count("agent_message"), 2);
    assert_eq!(event_count("task_complete"), 2);
}

#[test]
fn materializes_canonical_claude_layout() {
    let session = load_session(
        &fixture("codex_current_sample.jsonl"),
        Some(SessionFormat::Codex),
    )
    .unwrap();
    let temp = tempdir().unwrap();
    let path = materialize(&session, SessionFormat::Claude, temp.path()).unwrap();

    assert!(path.to_string_lossy().contains("/projects/"));
    assert!(temp.path().join("history.jsonl").exists());

    let mut saw_image = false;
    let mut saw_freeform_tool = false;
    let mut saw_structured_tool_result = false;
    let mut saw_failed_tool_result = false;
    for line in jsonl(&path) {
        assert!(line["version"].as_str().is_some_and(is_semver));
        assert_eq!(line["entrypoint"].as_str(), Some("cli"));
        let content = line["message"]["content"].as_array().unwrap();

        for block in content {
            // Codex block kinds must not leak into a Claude session.
            assert!(!matches!(
                block["type"].as_str(),
                Some("input_text" | "output_text")
            ));
            match block["type"].as_str() {
                Some("image") => saw_image = true,
                Some("tool_use") if block["name"] == "exec" => {
                    assert!(block["input"].is_object());
                    saw_freeform_tool = true;
                }
                Some("tool_result") => {
                    if block["is_error"] == true {
                        saw_failed_tool_result = true;
                    }
                    if let Some(items) = block["content"].as_array() {
                        assert!(items.iter().all(|item| {
                            matches!(item["type"].as_str(), Some("text" | "image" | "document"))
                        }));
                        saw_structured_tool_result = true;
                    }
                }
                _ => {}
            }
        }
    }
    assert!(saw_image);
    assert!(saw_freeform_tool);
    assert!(saw_structured_tool_result);
    assert!(saw_failed_tool_result);
}

#[test]
fn writes_ir_json() {
    let session = load_session(&fixture("claude_sample.jsonl"), None).unwrap();
    let temp = tempdir().unwrap();
    let output = temp.path().join("session.json");
    let path = materialize(&session, SessionFormat::Ir, &output).unwrap();
    assert!(
        fs::read_to_string(path)
            .unwrap()
            .contains("\"ir_version\": \"transession/v1\"")
    );
}

#[test]
fn auto_detects_pretty_printed_ir() {
    let temp = tempdir().unwrap();
    let input = temp.path().join("session.json");
    fs::write(
        &input,
        "{\n  \"ir_version\": \"transession/v1\",\n  \"metadata\": { \"session_id\": \"test\" },\n  \"events\": []\n}",
    )
    .unwrap();

    assert_eq!(detect_format(&input).unwrap(), SessionFormat::Ir);
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
    assert!(
        fs::read_to_string(path)
            .unwrap()
            .contains("[transession imported developer message]")
    );
}

#[test]
fn resolves_codex_session_ids_from_default_store_roots() {
    let session = load_session(&fixture("codex_sample.jsonl"), None).unwrap();
    let temp = tempdir().unwrap();
    materialize(&session, SessionFormat::Codex, temp.path()).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_transession"))
        .args([
            "inspect",
            "019cd6bd-10df-7e61-8506-e9ac5bdf4e6e",
            "--from",
            "codex",
            "--json",
        ])
        .env("TRANSESSION_CODEX_HOME", temp.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("\"detected_format\": \"codex\""));
}

/// Claude's store root moves between three environment variables; each one has
/// to resolve a bare session id.
#[test]
fn resolves_claude_session_ids_from_store_root_variables() {
    let session = load_session(&fixture("claude_sample.jsonl"), None).unwrap();

    for variable in [
        "TRANSESSION_CLAUDE_HOME",
        "CLAUDE_CONFIG_DIR",
        "CLAUDE_HOME",
    ] {
        let temp = tempdir().unwrap();
        materialize(&session, SessionFormat::Claude, temp.path()).unwrap();

        let output = Command::new(env!("CARGO_BIN_EXE_transession"))
            .args([
                "inspect",
                "d89e26cd-11f2-47e8-bea5-a73ad5458483",
                "--from",
                "claude",
                "--json",
            ])
            .env_remove("TRANSESSION_CLAUDE_HOME")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CLAUDE_HOME")
            .env(variable, temp.path())
            .output()
            .unwrap();

        assert!(output.status.success(), "{variable}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("\"detected_format\": \"claude\"")
        );
    }
}

#[test]
fn quick_cli_converts_by_session_id_and_prints_resume_hint() {
    let session = load_session(&fixture("claude_sample.jsonl"), None).unwrap();
    let source_home = tempdir().unwrap();
    let target_home = tempdir().unwrap();
    materialize(&session, SessionFormat::Claude, source_home.path()).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_transession"))
        .args([
            "--from",
            "claude",
            "--to",
            "codex",
            "d89e26cd-11f2-47e8-bea5-a73ad5458483",
            "--no-open",
        ])
        .env("TRANSESSION_CLAUDE_HOME", source_home.path())
        .env("TRANSESSION_CODEX_HOME", target_home.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(stdout.contains("created codex session:"));
    assert!(stdout.contains("resume with: codex resume "));
}

#[test]
fn quick_cli_opens_claude_target_by_default() {
    let mut session = load_session(&fixture("codex_sample.jsonl"), None).unwrap();
    let source_home = tempdir().unwrap();
    let target_home = tempdir().unwrap();
    session.metadata.cwd = Some(target_home.path().join("missing-session-cwd"));
    materialize(&session, SessionFormat::Codex, source_home.path()).unwrap();

    let log = target_home.path().join("launcher.log");
    let script = fake_cli(
        &target_home,
        "fake-claude.sh",
        &format!(
            "printf '%s\\n' \"$@\" > \"{log}\"\n\
             printf 'CLAUDE_CONFIG_DIR=%s\\nCLAUDE_HOME=%s\\n' \"$CLAUDE_CONFIG_DIR\" \"$CLAUDE_HOME\" >> \"{log}\"\n",
            log = log.display()
        ),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_transession"))
        .args([
            "--from",
            "codex",
            "--to",
            "claude",
            "--keep-session-id",
            "019cd6bd-10df-7e61-8506-e9ac5bdf4e6e",
        ])
        .arg("--output")
        .arg(target_home.path())
        .env("TRANSESSION_CODEX_HOME", source_home.path())
        .env("TRANSESSION_CLAUDE_BIN", &script)
        .output()
        .unwrap();

    assert!(output.status.success());
    let log = fs::read_to_string(log).unwrap();
    assert!(log.contains("-r"));
    assert!(log.contains("019cd6bd-10df-7e61-8506-e9ac5bdf4e6e"));
    assert!(log.contains(&format!(
        "CLAUDE_CONFIG_DIR={}",
        target_home.path().display()
    )));
    assert!(log.contains(&format!("CLAUDE_HOME={}", target_home.path().display())));
}

/// Claude Code keeps its account record in `<CLAUDE_CONFIG_DIR>/.claude.json`,
/// so overriding the variable with the store we just wrote into would send the
/// user through login again.
#[test]
fn quick_cli_opens_claude_in_installed_home_without_config_override() {
    let mut session = load_session(&fixture("codex_sample.jsonl"), None).unwrap();
    let source_home = tempdir().unwrap();
    let claude_home = tempdir().unwrap();
    session.metadata.cwd = Some(claude_home.path().join("missing-session-cwd"));
    materialize(&session, SessionFormat::Codex, source_home.path()).unwrap();

    let log = claude_home.path().join("launcher.log");
    let script = fake_cli(
        &claude_home,
        "fake-claude.sh",
        &format!(
            "printf 'CLAUDE_CONFIG_DIR=%s\\nCLAUDE_HOME=%s\\n' \"$CLAUDE_CONFIG_DIR\" \"$CLAUDE_HOME\" > \"{}\"\n",
            log.display()
        ),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_transession"))
        .args([
            "--from",
            "codex",
            "--to",
            "claude",
            "--keep-session-id",
            "019cd6bd-10df-7e61-8506-e9ac5bdf4e6e",
        ])
        .arg("--output")
        .arg(claude_home.path())
        .env("TRANSESSION_CODEX_HOME", source_home.path())
        .env("TRANSESSION_CLAUDE_HOME", claude_home.path())
        .env("TRANSESSION_CLAUDE_BIN", &script)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(log).unwrap(),
        "CLAUDE_CONFIG_DIR=\nCLAUDE_HOME=\n"
    );
}

#[test]
fn quick_cli_links_claude_login_state_into_a_custom_home() {
    let mut session = load_session(&fixture("codex_sample.jsonl"), None).unwrap();
    let source_home = tempdir().unwrap();
    let target_home = tempdir().unwrap();
    let installed_home = tempdir().unwrap();
    session.metadata.cwd = Some(target_home.path().join("missing-session-cwd"));
    materialize(&session, SessionFormat::Codex, source_home.path()).unwrap();
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

    let log = target_home.path().join("launcher.log");
    let script = fake_cli(
        &target_home,
        "fake-claude.sh",
        &format!(
            "for file in .credentials.json .claude.json; do\n\
               [ -e \"$CLAUDE_CONFIG_DIR/$file\" ] || {{ echo \"missing $file\" >&2; exit 1; }}\n\
             done\n\
             printf '%s\\n' \"$@\" > \"{}\"\n",
            log.display()
        ),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_transession"))
        .args([
            "--from",
            "codex",
            "--to",
            "claude",
            "--keep-session-id",
            "019cd6bd-10df-7e61-8506-e9ac5bdf4e6e",
        ])
        .arg("--output")
        .arg(target_home.path())
        .env("TRANSESSION_CODEX_HOME", source_home.path())
        .env("TRANSESSION_CLAUDE_HOME", installed_home.path())
        .env("TRANSESSION_CLAUDE_BIN", &script)
        .output()
        .unwrap();

    assert!(output.status.success(), "{output:?}");
    assert!(
        fs::read_to_string(log)
            .unwrap()
            .contains("019cd6bd-10df-7e61-8506-e9ac5bdf4e6e")
    );
}

#[test]
fn quick_cli_links_codex_auth_into_a_custom_home() {
    let mut session = load_session(&fixture("claude_sample.jsonl"), None).unwrap();
    let source_home = tempdir().unwrap();
    let target_home = tempdir().unwrap();
    let installed_home = tempdir().unwrap();
    session.metadata.cwd = Some(target_home.path().join("missing-session-cwd"));
    materialize(&session, SessionFormat::Claude, source_home.path()).unwrap();
    fs::write(
        installed_home.path().join("auth.json"),
        "{\"access_token\":\"test\"}",
    )
    .unwrap();

    let log = target_home.path().join("launcher.log");
    let script = fake_cli(
        &target_home,
        "fake-codex.sh",
        &format!(
            "[ -e \"$CODEX_HOME/auth.json\" ] || {{ echo 'missing auth' >&2; exit 1; }}\n\
             printf '%s\\n' \"$@\" > \"{log}\"\n\
             printf 'CODEX_HOME=%s\\n' \"$CODEX_HOME\" >> \"{log}\"\n",
            log = log.display()
        ),
    );

    let output = Command::new(env!("CARGO_BIN_EXE_transession"))
        .args([
            "--from",
            "claude",
            "--to",
            "codex",
            "--keep-session-id",
            "d89e26cd-11f2-47e8-bea5-a73ad5458483",
        ])
        .arg("--output")
        .arg(target_home.path())
        .env("TRANSESSION_CLAUDE_HOME", source_home.path())
        .env("CODEX_HOME", installed_home.path())
        .env("TRANSESSION_CODEX_BIN", &script)
        .output()
        .unwrap();

    assert!(output.status.success());
    let log = fs::read_to_string(log).unwrap();
    assert!(log.contains("resume"));
    assert!(log.contains("d89e26cd-11f2-47e8-bea5-a73ad5458483"));
    assert!(log.contains(&format!("CODEX_HOME={}", target_home.path().display())));
}
