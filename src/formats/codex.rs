use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Local, Utc};
use rusqlite::{Connection, params};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::{
    derive_title, first_user_text, json_to_string, normalize_block, parse_datetime, rfc3339,
    update_time_bounds, write_json_line,
};
use crate::ir::{
    ContentBlock, MessageEvent, ReasoningEvent, SessionEvent, SessionFormat, SessionMetadata,
    ToolCallEvent, ToolResultEvent, UniversalSession,
};

// Codex 0.147 writes the provider id in lowercase; the state DB and the resume
// picker compare it verbatim, so match that spelling.
const CODEX_MODEL_PROVIDER: &str = "openai";

pub fn load(path: &Path) -> Result<UniversalSession> {
    let file = File::open(path)
        .with_context(|| format!("failed to open Codex session {}", path.display()))?;

    let mut session = UniversalSession::new(Uuid::now_v7().to_string());
    session.metadata.source_format = Some(SessionFormat::Codex);

    for line in BufReader::new(file).lines() {
        let line = line.with_context(|| format!("failed to read {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }

        let value: Value = serde_json::from_str(&line)
            .with_context(|| format!("invalid JSONL in {}", path.display()))?;

        let timestamp = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_datetime);
        update_time_bounds(&mut session.metadata, timestamp);

        match value.get("type").and_then(Value::as_str) {
            Some("session_meta") => import_session_meta(&mut session.metadata, &value),
            Some("turn_context") => import_turn_context(&mut session.metadata, &value),
            Some("response_item") => import_response_item(&mut session.events, &value, timestamp),
            _ => {}
        }
    }

    if session.metadata.title.is_none() {
        session.metadata.title = first_user_text(&session);
    }

    Ok(session)
}

fn import_session_meta(metadata: &mut SessionMetadata, value: &Value) {
    let Some(payload) = value.get("payload").and_then(Value::as_object) else {
        return;
    };

    if let Some(id) = payload.get("id").and_then(Value::as_str) {
        metadata.session_id = id.to_string();
    }
    metadata.original_session_id = Some(metadata.session_id.clone());
    metadata.source_format = Some(SessionFormat::Codex);
    metadata.created_at = payload
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_datetime)
        .or(metadata.created_at);
    metadata.cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| metadata.cwd.clone());
    metadata.platform_version = payload
        .get("cli_version")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| metadata.platform_version.clone());

    for key in ["source", "model_provider", "originator"] {
        copy_extra(payload, metadata, key);
    }
    if let Some(text) = payload
        .get("base_instructions")
        .and_then(|value| value.get("text"))
    {
        metadata
            .extra
            .insert("codex_base_instructions".to_string(), text.clone());
    }

    // Codex records the repository state in session_meta. Keeping the whole
    // block lets us re-export it verbatim, while the branch also feeds the
    // Claude `gitBranch` field and the Codex picker column.
    if let Some(git) = payload.get("git") {
        if let Some(branch) = git.get("branch").and_then(Value::as_str) {
            metadata.git_branch = Some(branch.to_string());
        }
        metadata.extra.insert("codex_git".to_string(), git.clone());
    }
}

fn import_turn_context(metadata: &mut SessionMetadata, value: &Value) {
    let Some(payload) = value.get("payload").and_then(Value::as_object) else {
        return;
    };

    metadata.cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| metadata.cwd.clone());
    if let Some(model) = payload.get("model").and_then(Value::as_str) {
        metadata.model = Some(model.to_string());
    }

    for key in [
        "personality",
        "approval_policy",
        "sandbox_policy",
        "collaboration_mode",
        "user_instructions",
        "timezone",
        "current_date",
    ] {
        copy_extra(payload, metadata, key);
    }
}

fn copy_extra(payload: &Map<String, Value>, metadata: &mut SessionMetadata, key: &str) {
    if let Some(value) = payload.get(key) {
        metadata.extra.insert(format!("codex_{key}"), value.clone());
    }
}

fn import_response_item(
    events: &mut Vec<SessionEvent>,
    value: &Value,
    timestamp: Option<DateTime<Utc>>,
) {
    let Some(payload) = value.get("payload").and_then(Value::as_object) else {
        return;
    };
    let Some(payload_type) = payload.get("type").and_then(Value::as_str) else {
        return;
    };
    let id = payload
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string);

    match payload_type {
        "message" => {
            let blocks: Vec<ContentBlock> = payload
                .get("content")
                .and_then(Value::as_array)
                .map(|items| items.iter().map(normalize_block).collect())
                .unwrap_or_default();
            if blocks.is_empty() {
                return;
            }
            events.push(SessionEvent::Message(MessageEvent {
                id,
                parent_id: None,
                role: payload
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("assistant")
                    .to_string(),
                timestamp,
                blocks,
                metadata: BTreeMap::new(),
            }));
        }
        "reasoning" => {
            let summary: Vec<String> = payload
                .get("summary")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.get("text").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            if summary.is_empty() {
                return;
            }
            events.push(SessionEvent::Reasoning(ReasoningEvent {
                id,
                parent_id: None,
                timestamp,
                summary,
                metadata: BTreeMap::new(),
            }));
        }
        // Freeform (`custom_tool_call`) tools put their payload in `input`
        // instead of `arguments`, and both spell it as an embedded JSON string.
        "function_call" | "custom_tool_call" => {
            events.push(SessionEvent::ToolCall(ToolCallEvent {
                id,
                parent_id: None,
                call_id: string_field(payload, "call_id"),
                name: payload
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string(),
                timestamp,
                arguments: payload
                    .get("arguments")
                    .or_else(|| payload.get("input"))
                    .map(|value| match value {
                        Value::String(text) => {
                            serde_json::from_str(text).unwrap_or_else(|_| value.clone())
                        }
                        value => value.clone(),
                    })
                    .unwrap_or(Value::Null),
                metadata: BTreeMap::new(),
            }))
        }
        "function_call_output" | "custom_tool_call_output" => {
            let output = payload.get("output");
            events.push(SessionEvent::ToolResult(ToolResultEvent {
                id,
                parent_id: None,
                call_id: string_field(payload, "call_id"),
                timestamp,
                is_error: output.is_some_and(output_is_error),
                output: output.cloned().unwrap_or(Value::String(String::new())),
                metadata: BTreeMap::new(),
            }))
        }
        _ => {}
    }
}

// ==============================================================================
// Failure Detection
// ==============================================================================

// Codex has no `is_error` flag on a tool result: its shell tools state the
// outcome in the first line of their own output, and the structured envelope
// carries the process exit code. Claude does have the flag and renders failed
// results differently, so it is worth recovering.
//
// ponytail: only these three conventions are recognised. Free-form failures
// ("apply_patch verification failed: ...", "collab spawn failed: ...") are left
// as successes rather than growing a list of prose prefixes to match.

/// Codex output is either a plain string, or a list of content blocks whose
/// first text block holds the status line.
fn output_is_error(output: &Value) -> bool {
    match output {
        Value::String(text) => text_reports_failure(text),
        Value::Array(items) => items
            .iter()
            .find_map(|item| item.get("text").and_then(Value::as_str))
            .is_some_and(text_reports_failure),
        _ => false,
    }
}

fn text_reports_failure(text: &str) -> bool {
    // Some tools wrap their result as `{"metadata": {"exit_code": N, ...}, ...}`,
    // which states the outcome outright.
    if let Ok(value) = serde_json::from_str::<Value>(text)
        && let Some(exit_code) = value
            .get("metadata")
            .and_then(|metadata| metadata.get("exit_code"))
            .and_then(Value::as_i64)
    {
        return exit_code != 0;
    }

    // Otherwise the first line is `Exit code: N` or `Script completed`/
    // `Script failed`, depending on which shell tool ran.
    let first_line = text.lines().next().unwrap_or_default().trim();
    match first_line.strip_prefix("Exit code:") {
        Some(code) => code.trim().parse::<i64>().is_ok_and(|code| code != 0),
        None => first_line == "Script failed",
    }
}

fn string_field(payload: &Map<String, Value>, key: &str) -> String {
    payload
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

pub fn write(session: &UniversalSession, output: &Path) -> Result<PathBuf> {
    let session_id = codex_session_id(&session.metadata.session_id);
    let (created_at, updated_at) = time_bounds(session);
    let cwd = session
        .metadata
        .cwd
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));

    // A `.jsonl` output is a standalone file; anything else is a Codex home,
    // which also needs the thread registered so the resume picker finds it.
    let native_store = output.extension().and_then(|ext| ext.to_str()) != Some("jsonl");
    let session_file = if native_store {
        let local = created_at.with_timezone(&Local);
        output
            .join("sessions")
            .join(format!("{:04}", local.year()))
            .join(format!("{:02}", local.month()))
            .join(format!("{:02}", local.day()))
            .join(format!(
                "rollout-{}-{session_id}.jsonl",
                local.format("%Y-%m-%dT%H-%M-%S")
            ))
    } else {
        output.to_path_buf()
    };

    if let Some(parent) = session_file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut file = File::create(&session_file).with_context(|| {
        format!(
            "failed to create Codex session file {}",
            session_file.display()
        )
    })?;

    write_json_line(
        &mut file,
        &json!({
            "timestamp": rfc3339(created_at),
            "type": "session_meta",
            "payload": session_meta_payload(session, &session_id, &cwd, created_at),
        }),
    )?;

    // Codex replays a session as a sequence of turns: a user message closes the
    // previous turn and opens a new one, and every other event joins whichever
    // turn is open (starting one if the session begins mid-conversation).
    let mut active_turn: Option<ActiveTurn> = None;

    for event in &session.events {
        let timestamp = event.timestamp().unwrap_or(updated_at);
        let at = rfc3339(timestamp);

        match event {
            SessionEvent::Message(message) if message.role == "user" => {
                close_turn(&mut file, &mut active_turn, updated_at)?;
                active_turn = Some(start_turn(&mut file, timestamp)?);
                write_message_response_item(&mut file, message, &at)?;

                let images: Vec<String> =
                    message.blocks.iter().filter_map(codex_image_url).collect();
                let text = render_message_text(message);
                if text.is_some() || !images.is_empty() {
                    write_json_line(
                        &mut file,
                        &json!({
                            "timestamp": at,
                            "type": "event_msg",
                            "payload": {
                                "type": "user_message",
                                "message": text.unwrap_or_default(),
                                "images": images,
                                "local_images": [],
                                "text_elements": [],
                            }
                        }),
                    )?;
                }
            }
            SessionEvent::Message(message) => {
                // Developer messages carry repo instructions rather than a turn
                // of their own, so they never open one.
                if message.role != "developer" {
                    ensure_turn(&mut file, &mut active_turn, timestamp)?;
                }

                if message.role == "assistant"
                    && let Some(text) = render_message_text(message)
                {
                    write_json_line(
                        &mut file,
                        &json!({
                            "timestamp": at,
                            "type": "event_msg",
                            "payload": {
                                "type": "agent_message",
                                "message": text,
                                "phase": "commentary",
                            }
                        }),
                    )?;
                    if let Some(turn) = &mut active_turn {
                        turn.last_agent_message = Some(text);
                    }
                }

                write_message_response_item(&mut file, message, &at)?;
            }
            SessionEvent::Reasoning(reasoning) => {
                ensure_turn(&mut file, &mut active_turn, timestamp)?;

                let summary_text = join_non_empty(reasoning.summary.iter().map(String::as_str));
                if !summary_text.is_empty() {
                    write_json_line(
                        &mut file,
                        &json!({
                            "timestamp": at,
                            "type": "event_msg",
                            "payload": { "type": "agent_reasoning", "text": summary_text }
                        }),
                    )?;
                }

                write_json_line(
                    &mut file,
                    &json!({
                        "timestamp": at,
                        "type": "response_item",
                        "payload": {
                            "type": "reasoning",
                            "summary": reasoning
                                .summary
                                .iter()
                                .map(|text| json!({ "type": "summary_text", "text": text }))
                                .collect::<Vec<_>>(),
                        }
                    }),
                )?;
            }
            SessionEvent::ToolCall(call) => {
                ensure_turn(&mut file, &mut active_turn, timestamp)?;
                write_json_line(
                    &mut file,
                    &json!({
                        "timestamp": at,
                        "type": "response_item",
                        "payload": {
                            "type": "function_call",
                            "id": call.id.clone().unwrap_or_else(|| Uuid::now_v7().to_string()),
                            "name": call.name,
                            "call_id": call.call_id,
                            "arguments": json_to_string(&call.arguments),
                        }
                    }),
                )?;
            }
            SessionEvent::ToolResult(result) => {
                ensure_turn(&mut file, &mut active_turn, timestamp)?;
                write_json_line(
                    &mut file,
                    &json!({
                        "timestamp": at,
                        "type": "response_item",
                        "payload": {
                            "type": "function_call_output",
                            "call_id": result.call_id,
                            "output": json_to_string(&result.output),
                        }
                    }),
                )?;
            }
        }

        if let Some(turn) = &mut active_turn {
            turn.last_timestamp = Some(timestamp);
        }
    }

    close_turn(&mut file, &mut active_turn, updated_at)?;

    if native_store {
        let title = derive_title(session).unwrap_or_else(|| session_id.clone());
        let index_path = output.join("session_index.jsonl");
        let mut index = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&index_path)
            .with_context(|| format!("failed to open {}", index_path.display()))?;
        write_json_line(
            &mut index,
            &json!({
                "id": session_id,
                "thread_name": title,
                "updated_at": rfc3339(updated_at),
            }),
        )?;

        register_thread_in_sqlite(
            output,
            session,
            &session_file,
            &session_id,
            &title,
            created_at,
            updated_at,
        )?;
    }

    Ok(session_file)
}

fn session_meta_payload(
    session: &UniversalSession,
    session_id: &str,
    cwd: &Path,
    created_at: DateTime<Utc>,
) -> Value {
    let metadata = &session.metadata;
    let mut payload = json!({
        "id": session_id,
        "session_id": session_id,
        "timestamp": rfc3339(created_at),
        "cwd": cwd.display().to_string(),
        "originator": extra_string(metadata, "codex_originator")
            .unwrap_or_else(|| "transession".to_string()),
        "cli_version": super::codex_cli_version(),
        "source": metadata.extra.get("codex_source").cloned().unwrap_or_else(|| json!("cli")),
        "model_provider": CODEX_MODEL_PROVIDER,
        "thread_source": "user",
        "history_mode": "legacy",
    });

    if let Some(instructions) = extra_string(metadata, "codex_base_instructions") {
        payload["base_instructions"] = json!({ "text": instructions });
    }
    // An imported Codex session carries the original git block; sessions
    // arriving from Claude only know the branch name, which is still enough for
    // the resume picker's branch column.
    match (metadata.extra.get("codex_git"), &metadata.git_branch) {
        (Some(git), _) => payload["git"] = git.clone(),
        (None, Some(branch)) => payload["git"] = json!({ "branch": branch }),
        (None, None) => {}
    }

    payload
}

fn time_bounds(session: &UniversalSession) -> (DateTime<Utc>, DateTime<Utc>) {
    let event_times = || session.events.iter().filter_map(SessionEvent::timestamp);
    let created_at = session
        .metadata
        .created_at
        .or_else(|| event_times().min())
        .unwrap_or_else(Utc::now);
    let updated_at = session
        .metadata
        .updated_at
        .or_else(|| event_times().max())
        .unwrap_or(created_at);
    (created_at, updated_at)
}

struct ActiveTurn {
    turn_id: String,
    last_agent_message: Option<String>,
    last_timestamp: Option<DateTime<Utc>>,
}

fn ensure_turn(
    file: &mut impl Write,
    active_turn: &mut Option<ActiveTurn>,
    timestamp: DateTime<Utc>,
) -> Result<()> {
    if active_turn.is_none() {
        *active_turn = Some(start_turn(file, timestamp)?);
    }
    Ok(())
}

fn start_turn(file: &mut impl Write, timestamp: DateTime<Utc>) -> Result<ActiveTurn> {
    let turn_id = Uuid::now_v7().to_string();
    write_json_line(
        file,
        &json!({
            "timestamp": rfc3339(timestamp),
            "type": "event_msg",
            "payload": {
                "type": "task_started",
                "turn_id": turn_id,
                "model_context_window": 950000,
                "collaboration_mode_kind": "default",
            }
        }),
    )?;

    Ok(ActiveTurn {
        turn_id,
        last_agent_message: None,
        last_timestamp: Some(timestamp),
    })
}

fn close_turn(
    file: &mut impl Write,
    active_turn: &mut Option<ActiveTurn>,
    fallback: DateTime<Utc>,
) -> Result<()> {
    let Some(turn) = active_turn.take() else {
        return Ok(());
    };

    write_json_line(
        file,
        &json!({
            "timestamp": rfc3339(turn.last_timestamp.unwrap_or(fallback)),
            "type": "event_msg",
            "payload": {
                "type": "task_complete",
                "turn_id": turn.turn_id,
                "last_agent_message": turn.last_agent_message.unwrap_or_default(),
            }
        }),
    )
}

fn write_message_response_item(
    file: &mut impl Write,
    message: &MessageEvent,
    timestamp: &str,
) -> Result<()> {
    let blocks = message
        .blocks
        .iter()
        .filter_map(|block| {
            let mut object = Map::new();
            if let Some(text) = &block.text {
                object.insert(
                    "type".to_string(),
                    codex_block_kind(&message.role, &block.kind).into(),
                );
                object.insert("text".to_string(), text.clone().into());
                if let Some(Value::Object(extra)) = &block.data {
                    object.extend(extra.clone());
                }
            } else {
                object.insert("type".to_string(), "input_image".into());
                object.insert("image_url".to_string(), codex_image_url(block)?.into());
            }
            Some(Value::Object(object))
        })
        .collect::<Vec<_>>();

    if blocks.is_empty() {
        return Ok(());
    }

    write_json_line(
        file,
        &json!({
            "timestamp": timestamp,
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": message.role,
                "content": blocks,
            }
        }),
    )
}

fn codex_image_url(block: &ContentBlock) -> Option<String> {
    let data = block.data.as_ref()?;
    if block.kind == "input_image" {
        return data
            .get("image_url")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    if block.kind != "image" {
        return None;
    }

    let source = data.get("source")?;
    match source.get("type").and_then(Value::as_str) {
        Some("base64") => Some(format!(
            "data:{};base64,{}",
            source.get("media_type")?.as_str()?,
            source.get("data")?.as_str()?
        )),
        Some("url") => source
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

fn render_message_text(message: &MessageEvent) -> Option<String> {
    let text = join_non_empty(
        message
            .blocks
            .iter()
            .filter_map(|block| block.text.as_deref()),
    );
    (!text.is_empty()).then_some(text)
}

fn join_non_empty<'a>(parts: impl Iterator<Item = &'a str>) -> String {
    parts
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn codex_block_kind(role: &str, kind: &str) -> &'static str {
    match kind {
        "input_text" => "input_text",
        "output_text" => "output_text",
        _ if role == "assistant" => "output_text",
        _ => "input_text",
    }
}

fn extra_string(metadata: &SessionMetadata, key: &str) -> Option<String> {
    metadata
        .extra
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn codex_session_id(candidate: &str) -> String {
    if Uuid::parse_str(candidate).is_ok() {
        candidate.to_string()
    } else {
        Uuid::now_v7().to_string()
    }
}

/// Codex lists resumable threads from `state_5.sqlite` rather than the rollout
/// files, so a translated session stays invisible until it has a row here.
fn register_thread_in_sqlite(
    codex_root: &Path,
    session: &UniversalSession,
    session_file: &Path,
    session_id: &str,
    title: &str,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
) -> Result<()> {
    let sqlite_path = codex_root.join("state_5.sqlite");
    if !sqlite_path.exists() {
        return Ok(());
    }

    let connection = Connection::open(&sqlite_path)
        .with_context(|| format!("failed to open {}", sqlite_path.display()))?;
    let first_user_message = first_user_text(session).unwrap_or_else(|| title.to_string());
    let cwd = session
        .metadata
        .cwd
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| ".".to_string());
    let sandbox_policy = session
        .metadata
        .extra
        .get("codex_sandbox_policy")
        .map(json_to_string)
        .unwrap_or_else(|| json!({ "type": "workspace-write" }).to_string());
    let approval_mode = extra_string(&session.metadata, "codex_approval_policy")
        .unwrap_or_else(|| "on-request".to_string());
    let has_user_event = session
        .events
        .iter()
        .any(|event| matches!(event, SessionEvent::Message(message) if message.role == "user"))
        as i64;

    connection
        .execute(
            "INSERT INTO threads (
                id, rollout_path, created_at, updated_at, source, model_provider, cwd, title,
                sandbox_policy, approval_mode, tokens_used, has_user_event, archived, git_sha,
                git_branch, git_origin_url, cli_version, first_user_message, agent_nickname,
                agent_role, memory_mode
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0, ?11, 0, NULL, ?12, NULL, ?13, ?14, NULL, NULL, 'enabled'
            )
            ON CONFLICT(id) DO UPDATE SET
                rollout_path=excluded.rollout_path,
                updated_at=excluded.updated_at,
                source=excluded.source,
                model_provider=excluded.model_provider,
                cwd=excluded.cwd,
                title=excluded.title,
                sandbox_policy=excluded.sandbox_policy,
                approval_mode=excluded.approval_mode,
                has_user_event=excluded.has_user_event,
                git_branch=excluded.git_branch,
                cli_version=excluded.cli_version,
                first_user_message=excluded.first_user_message,
                memory_mode=excluded.memory_mode",
            params![
                session_id,
                session_file.display().to_string(),
                created_at.timestamp(),
                updated_at.timestamp(),
                "cli",
                CODEX_MODEL_PROVIDER,
                cwd,
                title,
                sandbox_policy,
                approval_mode,
                has_user_event,
                session.metadata.git_branch,
                super::codex_cli_version(),
                first_user_message,
            ],
        )
        .with_context(|| {
            format!(
                "failed to register thread {session_id} in {}",
                sqlite_path.display()
            )
        })?;

    // Codex 0.144+ hides rows without a preview; older state DBs lack these
    // columns, so a failure here is not fatal.
    let _ = connection.execute(
        "UPDATE threads SET preview = ?1, thread_source = 'user', history_mode = 'legacy' WHERE id = ?2",
        params![first_user_message, session_id],
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::output_is_error;
    use serde_json::json;

    #[test]
    fn reads_failure_out_of_codex_tool_output() {
        // `Exit code: N` from the shell tool.
        assert!(!output_is_error(&json!(
            "Exit code: 0\nWall time: 0.1 seconds"
        )));
        assert!(output_is_error(&json!(
            "Exit code: 127\nWall time: 0.1 seconds"
        )));

        // `Script completed` / `Script failed` from the freeform exec tool.
        let block = |text| json!([{ "type": "input_text", "text": text }]);
        assert!(!output_is_error(&block(
            "Script completed\nWall time 0.0 seconds"
        )));
        assert!(output_is_error(&block(
            "Script failed\nWall time 0.0 seconds"
        )));

        // The structured envelope states the exit code outright.
        assert!(!output_is_error(&json!(
            r#"{"metadata":{"exit_code":0,"duration_seconds":0.0},"output":"done"}"#
        )));
        assert!(output_is_error(&json!(
            r#"{"metadata":{"exit_code":2,"duration_seconds":0.0},"output":"nope"}"#
        )));

        // Output with no status line of its own is not a failure.
        assert!(!output_is_error(&block("README.md contents")));
        assert!(!output_is_error(&json!("Plan updated")));
        assert!(!output_is_error(&json!({ "unexpected": true })));
    }
}
