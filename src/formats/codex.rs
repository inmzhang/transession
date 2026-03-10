use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Local, SecondsFormat, Utc};
use rusqlite::{Connection, params};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::ir::{
    ContentBlock, MessageEvent, ReasoningEvent, SessionEvent, SessionFormat, SessionMetadata,
    ToolCallEvent, ToolResultEvent, UniversalSession,
};

pub struct CodexMaterialization {
    pub session_file: PathBuf,
    pub session_index: Option<PathBuf>,
}

struct ActiveTurn {
    turn_id: String,
    last_agent_message: Option<String>,
    last_timestamp: Option<DateTime<Utc>>,
}

pub fn load(path: &Path) -> Result<UniversalSession> {
    let file = File::open(path)
        .with_context(|| format!("failed to open Codex session {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut session = UniversalSession::new(Uuid::now_v7().to_string());
    session.metadata.source_format = Some(SessionFormat::Codex);

    for line in reader.lines() {
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
            Some("response_item") => import_response_item(&mut session.events, &value),
            _ => {}
        }
    }

    if session.metadata.title.is_none() {
        session.metadata.title = derive_title(&session);
    }

    Ok(session)
}

fn import_session_meta(metadata: &mut SessionMetadata, value: &Value) {
    let payload = value.get("payload").and_then(Value::as_object);
    let Some(payload) = payload else {
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
    metadata.model = payload
        .get("model_provider")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| metadata.model.clone());
    metadata.platform_version = payload
        .get("cli_version")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| metadata.platform_version.clone());

    if let Some(source) = payload.get("source") {
        metadata
            .extra
            .insert("codex_source".to_string(), source.clone());
    }
    if let Some(originator) = payload.get("originator") {
        metadata
            .extra
            .insert("codex_originator".to_string(), originator.clone());
    }
    if let Some(base_instructions) = payload
        .get("base_instructions")
        .and_then(|value| value.get("text"))
    {
        metadata.extra.insert(
            "codex_base_instructions".to_string(),
            base_instructions.clone(),
        );
    }
}

fn import_turn_context(metadata: &mut SessionMetadata, value: &Value) {
    let payload = value.get("payload").and_then(Value::as_object);
    let Some(payload) = payload else {
        return;
    };

    metadata.cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| metadata.cwd.clone());

    if metadata.model.is_none() {
        metadata.model = payload
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string);
    }

    if let Some(personality) = payload.get("personality") {
        metadata
            .extra
            .insert("codex_personality".to_string(), personality.clone());
    }
    copy_if_present(
        payload,
        metadata,
        "approval_policy",
        "codex_approval_policy",
    );
    copy_if_present(payload, metadata, "sandbox_policy", "codex_sandbox_policy");
    copy_if_present(
        payload,
        metadata,
        "collaboration_mode",
        "codex_collaboration_mode",
    );
    copy_if_present(
        payload,
        metadata,
        "user_instructions",
        "codex_user_instructions",
    );
    copy_if_present(payload, metadata, "timezone", "codex_timezone");
    copy_if_present(payload, metadata, "current_date", "codex_current_date");
}

fn import_response_item(events: &mut Vec<SessionEvent>, value: &Value) {
    let payload = value.get("payload").cloned().unwrap_or(Value::Null);
    let Some(payload_type) = payload.get("type").and_then(Value::as_str) else {
        return;
    };
    let timestamp = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_datetime);

    match payload_type {
        "message" => import_message(events, payload, timestamp),
        "reasoning" => import_reasoning(events, payload, timestamp),
        "function_call" => import_tool_call(events, payload, timestamp),
        "function_call_output" => import_tool_result(events, payload, timestamp),
        _ => {}
    }
}

fn import_message(
    events: &mut Vec<SessionEvent>,
    payload: Value,
    timestamp: Option<DateTime<Utc>>,
) {
    let Some(payload_object) = payload.as_object() else {
        return;
    };

    let role = payload_object
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("assistant")
        .to_string();
    let blocks: Vec<ContentBlock> = payload_object
        .get("content")
        .and_then(Value::as_array)
        .map(|items| items.iter().map(normalize_block).collect())
        .unwrap_or_default();

    if blocks.is_empty() {
        return;
    }

    events.push(SessionEvent::Message(MessageEvent {
        id: payload_object
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string),
        parent_id: None,
        role,
        timestamp,
        blocks,
        metadata: BTreeMap::new(),
    }));
}

fn import_reasoning(
    events: &mut Vec<SessionEvent>,
    payload: Value,
    timestamp: Option<DateTime<Utc>>,
) {
    let Some(payload_object) = payload.as_object() else {
        return;
    };

    let summary: Vec<String> = payload_object
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
        id: payload_object
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string),
        parent_id: None,
        timestamp,
        summary,
        metadata: BTreeMap::new(),
    }));
}

fn import_tool_call(
    events: &mut Vec<SessionEvent>,
    payload: Value,
    timestamp: Option<DateTime<Utc>>,
) {
    let Some(payload_object) = payload.as_object() else {
        return;
    };

    let arguments = payload_object
        .get("arguments")
        .and_then(Value::as_str)
        .map(parse_jsonish)
        .unwrap_or(Value::Null);

    events.push(SessionEvent::ToolCall(ToolCallEvent {
        id: payload_object
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string),
        parent_id: None,
        call_id: payload_object
            .get("call_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        name: payload_object
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        timestamp,
        arguments,
        metadata: BTreeMap::new(),
    }));
}

fn import_tool_result(
    events: &mut Vec<SessionEvent>,
    payload: Value,
    timestamp: Option<DateTime<Utc>>,
) {
    let Some(payload_object) = payload.as_object() else {
        return;
    };

    let output = payload_object
        .get("output")
        .cloned()
        .unwrap_or(Value::String(String::new()));

    events.push(SessionEvent::ToolResult(ToolResultEvent {
        id: None,
        parent_id: None,
        call_id: payload_object
            .get("call_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        timestamp,
        output,
        is_error: false,
        metadata: BTreeMap::new(),
    }));
}

pub fn write(session: &UniversalSession, output: &Path) -> Result<PathBuf> {
    let materialization = plan_output(session, output);
    if let Some(parent) = materialization.session_file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let mut file = File::create(&materialization.session_file).with_context(|| {
        format!(
            "failed to create Codex session file {}",
            materialization.session_file.display()
        )
    })?;

    let session_id = codex_session_id(&session.metadata.session_id);
    let created_at = session
        .metadata
        .created_at
        .or_else(|| {
            session
                .events
                .iter()
                .filter_map(SessionEvent::timestamp)
                .min()
        })
        .unwrap_or_else(Utc::now);
    let updated_at = session
        .metadata
        .updated_at
        .or_else(|| {
            session
                .events
                .iter()
                .filter_map(SessionEvent::timestamp)
                .max()
        })
        .unwrap_or(created_at);
    let cwd = session
        .metadata
        .cwd
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));

    let mut session_meta_payload = Map::new();
    session_meta_payload.insert("id".to_string(), Value::String(session_id.clone()));
    session_meta_payload.insert(
        "timestamp".to_string(),
        Value::String(created_at.to_rfc3339_opts(SecondsFormat::Millis, true)),
    );
    session_meta_payload.insert("cwd".to_string(), Value::String(cwd.display().to_string()));
    session_meta_payload.insert(
        "originator".to_string(),
        extra_string(&session.metadata, "codex_originator")
            .unwrap_or_else(|| "transession".to_string())
            .into(),
    );
    session_meta_payload.insert(
        "cli_version".to_string(),
        codex_cli_version(&session.metadata).into(),
    );
    session_meta_payload.insert(
        "source".to_string(),
        session
            .metadata
            .extra
            .get("codex_source")
            .cloned()
            .unwrap_or_else(|| Value::String("import".to_string())),
    );
    session_meta_payload.insert(
        "model_provider".to_string(),
        Value::String(
            session
                .metadata
                .model
                .clone()
                .unwrap_or_else(|| "imported".to_string()),
        ),
    );
    session_meta_payload.insert(
        "base_instructions".to_string(),
        json!({
            "text": extra_string(&session.metadata, "codex_base_instructions").unwrap_or_else(|| {
                format!(
                    "Imported by transession from {} session {}.",
                    session
                        .metadata
                        .source_format
                        .map(format_name)
                        .unwrap_or("unknown"),
                    session
                        .metadata
                        .original_session_id
                        .clone()
                        .unwrap_or_else(|| session.metadata.session_id.clone()),
                )
            })
        }),
    );

    write_json_line(
        &mut file,
        &json!({
            "timestamp": created_at.to_rfc3339_opts(SecondsFormat::Millis, true),
            "type": "session_meta",
            "payload": session_meta_payload,
        }),
    )?;

    let mut active_turn: Option<ActiveTurn> = None;

    for event in &session.events {
        match event {
            SessionEvent::Message(message) => {
                let timestamp = message.timestamp.unwrap_or(updated_at);
                let rendered_text = render_message_text(message);

                if message.role == "user" {
                    close_turn(&mut file, &mut active_turn, updated_at)?;
                    active_turn = Some(start_turn(&mut file, &session.metadata, &cwd, timestamp)?);
                    write_message_response_item(&mut file, message, updated_at)?;
                    if let Some(text) = rendered_text {
                        write_json_line(
                            &mut file,
                            &json!({
                                "timestamp": event_timestamp(message.timestamp, updated_at),
                                "type": "event_msg",
                                "payload": {
                                    "type": "user_message",
                                    "message": text,
                                    "images": [],
                                    "local_images": [],
                                    "text_elements": [],
                                }
                            }),
                        )?;
                    }
                    if let Some(turn) = &mut active_turn {
                        turn.last_timestamp = Some(timestamp);
                    }
                    continue;
                }

                if message.role != "developer" && active_turn.is_none() {
                    active_turn = Some(start_turn(&mut file, &session.metadata, &cwd, timestamp)?);
                }

                if message.role == "assistant"
                    && let Some(text) = rendered_text.clone()
                {
                    write_json_line(
                        &mut file,
                        &json!({
                            "timestamp": event_timestamp(message.timestamp, updated_at),
                            "type": "event_msg",
                            "payload": {
                                "type": "agent_message",
                                "message": text,
                                "phase": "commentary",
                            }
                        }),
                    )?;
                    if let Some(turn) = &mut active_turn {
                        turn.last_agent_message = rendered_text;
                    }
                }

                write_message_response_item(&mut file, message, updated_at)?;
                if let Some(turn) = &mut active_turn {
                    turn.last_timestamp = Some(timestamp);
                }
            }
            SessionEvent::Reasoning(reasoning) => {
                let timestamp = reasoning.timestamp.unwrap_or(updated_at);
                if active_turn.is_none() {
                    active_turn = Some(start_turn(&mut file, &session.metadata, &cwd, timestamp)?);
                }

                let summary_text = render_reasoning_text(reasoning);
                if !summary_text.is_empty() {
                    write_json_line(
                        &mut file,
                        &json!({
                            "timestamp": event_timestamp(reasoning.timestamp, updated_at),
                            "type": "event_msg",
                            "payload": {
                                "type": "agent_reasoning",
                                "text": summary_text,
                            }
                        }),
                    )?;
                }

                write_json_line(
                    &mut file,
                    &json!({
                        "timestamp": event_timestamp(reasoning.timestamp, updated_at),
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
                if let Some(turn) = &mut active_turn {
                    turn.last_timestamp = Some(timestamp);
                }
            }
            SessionEvent::ToolCall(call) => {
                let timestamp = call.timestamp.unwrap_or(updated_at);
                if active_turn.is_none() {
                    active_turn = Some(start_turn(&mut file, &session.metadata, &cwd, timestamp)?);
                }
                write_json_line(
                    &mut file,
                    &json!({
                        "timestamp": event_timestamp(call.timestamp, updated_at),
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
                if let Some(turn) = &mut active_turn {
                    turn.last_timestamp = Some(timestamp);
                }
            }
            SessionEvent::ToolResult(result) => {
                let timestamp = result.timestamp.unwrap_or(updated_at);
                if active_turn.is_none() {
                    active_turn = Some(start_turn(&mut file, &session.metadata, &cwd, timestamp)?);
                }
                write_json_line(
                    &mut file,
                    &json!({
                        "timestamp": event_timestamp(result.timestamp, updated_at),
                        "type": "response_item",
                        "payload": {
                            "type": "function_call_output",
                            "call_id": result.call_id,
                            "output": json_to_string(&result.output),
                        }
                    }),
                )?;
                if let Some(turn) = &mut active_turn {
                    turn.last_timestamp = Some(timestamp);
                }
            }
        }
    }

    close_turn(&mut file, &mut active_turn, updated_at)?;

    if let Some(session_index) = &materialization.session_index {
        if let Some(parent) = session_index.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let mut index = OpenOptions::new()
            .create(true)
            .append(true)
            .open(session_index)
            .with_context(|| format!("failed to open {}", session_index.display()))?;

        write_json_line(
            &mut index,
            &json!({
                "id": session_id,
                "thread_name": derive_title(session).unwrap_or_else(|| "Imported session".to_string()),
                "updated_at": updated_at.to_rfc3339_opts(SecondsFormat::Millis, true),
            }),
        )?;
    }

    if output.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
        register_thread_in_sqlite(
            output,
            session,
            &materialization.session_file,
            &session_id,
            created_at,
            updated_at,
        )?;
    }

    Ok(materialization.session_file)
}

fn plan_output(session: &UniversalSession, output: &Path) -> CodexMaterialization {
    if output.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
        return CodexMaterialization {
            session_file: output.to_path_buf(),
            session_index: None,
        };
    }

    let created_at = session
        .metadata
        .created_at
        .unwrap_or_else(Utc::now)
        .with_timezone(&Local);
    let session_id = codex_session_id(&session.metadata.session_id);
    let relative = PathBuf::from("sessions")
        .join(format!("{:04}", created_at.year()))
        .join(format!("{:02}", created_at.month()))
        .join(format!("{:02}", created_at.day()))
        .join(format!(
            "rollout-{}-{}.jsonl",
            created_at.format("%Y-%m-%dT%H-%M-%S"),
            session_id
        ));

    CodexMaterialization {
        session_file: output.join(relative),
        session_index: Some(output.join("session_index.jsonl")),
    }
}

fn normalize_block(value: &Value) -> ContentBlock {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("text")
        .to_string();
    let text = ["text", "thinking", "content"]
        .iter()
        .find_map(|key| value.get(key).and_then(Value::as_str))
        .map(str::to_string);

    let mut object = value.as_object().cloned().unwrap_or_default();
    object.remove("type");
    object.remove("text");
    object.remove("thinking");
    object.remove("content");
    let data = (!object.is_empty()).then_some(Value::Object(object));

    ContentBlock { kind, text, data }
}

fn parse_datetime(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn parse_jsonish(value: &str) -> Value {
    serde_json::from_str(value).unwrap_or_else(|_| Value::String(value.to_string()))
}

fn json_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn event_timestamp(timestamp: Option<DateTime<Utc>>, fallback: DateTime<Utc>) -> String {
    timestamp
        .unwrap_or(fallback)
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn write_json_line(file: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *file, value).context("failed to encode JSONL line")?;
    file.write_all(b"\n").context("failed to write newline")
}

fn update_time_bounds(metadata: &mut SessionMetadata, timestamp: Option<DateTime<Utc>>) {
    let Some(timestamp) = timestamp else {
        return;
    };
    metadata.created_at = Some(match metadata.created_at {
        Some(current) => current.min(timestamp),
        None => timestamp,
    });
    metadata.updated_at = Some(match metadata.updated_at {
        Some(current) => current.max(timestamp),
        None => timestamp,
    });
}

fn derive_title(session: &UniversalSession) -> Option<String> {
    if let Some(title) = &session.metadata.title {
        return Some(title.clone());
    }

    session.events.iter().find_map(|event| {
        let SessionEvent::Message(message) = event else {
            return None;
        };
        if message.role != "user" {
            return None;
        }
        message
            .blocks
            .iter()
            .filter_map(|block| block.text.as_deref())
            .map(collapse_whitespace)
            .find(|text| !text.is_empty())
    })
}

fn collapse_whitespace(text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.chars().take(80).collect()
}

fn copy_if_present(
    payload: &serde_json::Map<String, Value>,
    metadata: &mut SessionMetadata,
    input_key: &str,
    output_key: &str,
) {
    if let Some(value) = payload.get(input_key) {
        metadata.extra.insert(output_key.to_string(), value.clone());
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

fn codex_cli_version(metadata: &SessionMetadata) -> String {
    if metadata.source_format == Some(SessionFormat::Codex) {
        metadata
            .platform_version
            .clone()
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
    } else {
        env!("CARGO_PKG_VERSION").to_string()
    }
}

fn local_timezone_name_or_offset() -> String {
    std::env::var("TZ").unwrap_or_else(|_| Local::now().offset().to_string())
}

fn start_turn(
    file: &mut impl Write,
    metadata: &SessionMetadata,
    cwd: &Path,
    timestamp: DateTime<Utc>,
) -> Result<ActiveTurn> {
    let turn_id = Uuid::now_v7().to_string();
    let rendered_timestamp = timestamp.to_rfc3339_opts(SecondsFormat::Millis, true);
    let collaboration_mode = metadata
        .extra
        .get("codex_collaboration_mode")
        .cloned()
        .unwrap_or_else(|| json!({ "mode": "default" }));
    let collaboration_mode_kind = collaboration_mode
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("default");

    write_json_line(
        file,
        &json!({
            "timestamp": rendered_timestamp,
            "type": "event_msg",
            "payload": {
                "type": "task_started",
                "turn_id": turn_id,
                "model_context_window": 950000,
                "collaboration_mode_kind": collaboration_mode_kind,
            }
        }),
    )?;
    write_json_line(
        file,
        &json!({
            "timestamp": rendered_timestamp,
            "type": "turn_context",
            "payload": build_turn_context_payload(metadata, cwd, &turn_id, timestamp),
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
            "timestamp": event_timestamp(turn.last_timestamp, fallback),
            "type": "event_msg",
            "payload": {
                "type": "task_complete",
                "turn_id": turn.turn_id,
                "last_agent_message": turn.last_agent_message.unwrap_or_default(),
            }
        }),
    )
}

fn build_turn_context_payload(
    metadata: &SessionMetadata,
    cwd: &Path,
    turn_id: &str,
    timestamp: DateTime<Utc>,
) -> Map<String, Value> {
    let mut turn_context_payload = Map::new();
    turn_context_payload.insert("turn_id".to_string(), Value::String(turn_id.to_string()));
    turn_context_payload.insert("cwd".to_string(), Value::String(cwd.display().to_string()));
    turn_context_payload.insert(
        "current_date".to_string(),
        extra_string(metadata, "codex_current_date")
            .unwrap_or_else(|| {
                timestamp
                    .with_timezone(&Local)
                    .format("%Y-%m-%d")
                    .to_string()
            })
            .into(),
    );
    turn_context_payload.insert(
        "timezone".to_string(),
        extra_string(metadata, "codex_timezone")
            .unwrap_or_else(local_timezone_name_or_offset)
            .into(),
    );
    turn_context_payload.insert(
        "approval_policy".to_string(),
        metadata
            .extra
            .get("codex_approval_policy")
            .cloned()
            .unwrap_or_else(|| Value::String("on-request".to_string())),
    );
    turn_context_payload.insert(
        "sandbox_policy".to_string(),
        metadata
            .extra
            .get("codex_sandbox_policy")
            .cloned()
            .unwrap_or_else(|| json!({ "type": "workspace-write" })),
    );
    turn_context_payload.insert(
        "model".to_string(),
        metadata
            .model
            .clone()
            .unwrap_or_else(|| "gpt-5".to_string())
            .into(),
    );
    turn_context_payload.insert(
        "personality".to_string(),
        metadata
            .extra
            .get("codex_personality")
            .cloned()
            .unwrap_or_else(|| Value::String("pragmatic".to_string())),
    );
    turn_context_payload.insert(
        "collaboration_mode".to_string(),
        metadata
            .extra
            .get("codex_collaboration_mode")
            .cloned()
            .unwrap_or_else(|| json!({ "mode": "default" })),
    );
    if let Some(user_instructions) = metadata.extra.get("codex_user_instructions") {
        turn_context_payload.insert("user_instructions".to_string(), user_instructions.clone());
    }
    turn_context_payload
}

fn write_message_response_item(
    file: &mut impl Write,
    message: &MessageEvent,
    fallback: DateTime<Utc>,
) -> Result<()> {
    let blocks = message
        .blocks
        .iter()
        .filter_map(|block| {
            let text = block.text.clone()?;
            let mapped_kind = codex_block_kind(&message.role, &block.kind);
            let mut object = Map::new();
            object.insert("type".to_string(), Value::String(mapped_kind.to_string()));
            object.insert("text".to_string(), Value::String(text));
            if let Some(Value::Object(extra)) = &block.data {
                object.extend(extra.clone());
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
            "timestamp": event_timestamp(message.timestamp, fallback),
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": message.role,
                "content": blocks,
            }
        }),
    )
}

fn render_message_text(message: &MessageEvent) -> Option<String> {
    let text = message
        .blocks
        .iter()
        .filter_map(|block| block.text.as_deref())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!text.is_empty()).then_some(text)
}

fn render_reasoning_text(reasoning: &ReasoningEvent) -> String {
    reasoning
        .summary
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn codex_block_kind(role: &str, original_kind: &str) -> &'static str {
    match original_kind {
        "input_text" => "input_text",
        "output_text" => "output_text",
        _ => {
            if role == "assistant" {
                "output_text"
            } else {
                "input_text"
            }
        }
    }
}

fn register_thread_in_sqlite(
    codex_root: &Path,
    session: &UniversalSession,
    session_file: &Path,
    session_id: &str,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
) -> Result<()> {
    let sqlite_path = codex_root.join("state_5.sqlite");
    if !sqlite_path.exists() {
        return Ok(());
    }

    let connection = Connection::open(&sqlite_path)
        .with_context(|| format!("failed to open {}", sqlite_path.display()))?;
    let title = derive_title(session).unwrap_or_else(|| "Imported session".to_string());
    let first_user_message = first_user_message(session).unwrap_or_else(|| title.clone());
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
        .unwrap_or_else(|| "{\"type\":\"workspace-write\"}".to_string());
    let approval_mode = extra_string(&session.metadata, "codex_approval_policy")
        .unwrap_or_else(|| "on-request".to_string());
    let model_provider = session
        .metadata
        .model
        .clone()
        .unwrap_or_else(|| "imported".to_string());
    let git_branch = session.metadata.git_branch.clone();
    let has_user_event = session
        .events
        .iter()
        .any(|event| matches!(event, SessionEvent::Message(message) if message.role == "user"))
        as i64;

    connection
        .execute(
            "INSERT INTO threads (
                id,
                rollout_path,
                created_at,
                updated_at,
                source,
                model_provider,
                cwd,
                title,
                sandbox_policy,
                approval_mode,
                tokens_used,
                has_user_event,
                archived,
                git_sha,
                git_branch,
                git_origin_url,
                cli_version,
                first_user_message,
                agent_nickname,
                agent_role,
                memory_mode
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
                model_provider,
                cwd,
                title,
                sandbox_policy,
                approval_mode,
                has_user_event,
                git_branch,
                codex_cli_version(&session.metadata),
                first_user_message,
            ],
        )
        .with_context(|| format!("failed to register thread {} in {}", session_id, sqlite_path.display()))?;

    Ok(())
}

fn first_user_message(session: &UniversalSession) -> Option<String> {
    session.events.iter().find_map(|event| {
        let SessionEvent::Message(message) = event else {
            return None;
        };
        if message.role != "user" {
            return None;
        }
        message
            .blocks
            .iter()
            .filter_map(|block| block.text.as_deref())
            .map(collapse_whitespace)
            .find(|text| !text.is_empty())
    })
}

fn format_name(format: SessionFormat) -> &'static str {
    match format {
        SessionFormat::Ir => "IR",
        SessionFormat::Codex => "Codex",
        SessionFormat::Claude => "Claude",
    }
}
