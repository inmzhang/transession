use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, Local, SecondsFormat, Utc};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::ir::{
    ContentBlock, MessageEvent, ReasoningEvent, SessionEvent, SessionFormat, SessionMetadata,
    ToolCallEvent, ToolResultEvent, UniversalSession,
};

pub struct CodexMaterialization {
    pub session_file: PathBuf,
    pub session_index: Option<PathBuf>,
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

    write_json_line(
        &mut file,
        &json!({
            "timestamp": created_at.to_rfc3339_opts(SecondsFormat::Millis, true),
            "type": "session_meta",
            "payload": {
                "id": session_id,
                "timestamp": created_at.to_rfc3339_opts(SecondsFormat::Millis, true),
                "cwd": cwd,
                "originator": "transession",
                "cli_version": env!("CARGO_PKG_VERSION"),
                "source": "import",
                "model_provider": session.metadata.model.clone().unwrap_or_else(|| "imported".to_string()),
                "base_instructions": {
                    "text": format!(
                        "Imported by transession from {} session {}.",
                        session
                            .metadata
                            .source_format
                            .map(format_name)
                            .unwrap_or("unknown"),
                        session.metadata.original_session_id.clone().unwrap_or_else(|| session.metadata.session_id.clone()),
                    )
                }
            }
        }),
    )?;

    write_json_line(
        &mut file,
        &json!({
            "timestamp": created_at.to_rfc3339_opts(SecondsFormat::Millis, true),
            "type": "turn_context",
            "payload": {
                "turn_id": Uuid::now_v7().to_string(),
                "cwd": cwd,
                "current_date": created_at.with_timezone(&Local).format("%Y-%m-%d").to_string(),
                "timezone": Local::now().offset().to_string(),
                "approval_policy": "on-request",
                "sandbox_policy": { "type": "workspace-write" },
                "model": session.metadata.model.clone().unwrap_or_else(|| "gpt-5".to_string()),
                "personality": "pragmatic",
                "collaboration_mode": { "mode": "default" }
            }
        }),
    )?;

    for event in &session.events {
        match event {
            SessionEvent::Message(message) => {
                if !matches!(message.role.as_str(), "user" | "assistant") {
                    continue;
                }

                let blocks = message
                    .blocks
                    .iter()
                    .filter_map(|block| {
                        let text = block.text.clone()?;
                        let mapped_kind = match message.role.as_str() {
                            "user" => "input_text",
                            _ => "output_text",
                        };
                        Some(json!({
                            "type": mapped_kind,
                            "text": text,
                        }))
                    })
                    .collect::<Vec<_>>();

                if blocks.is_empty() {
                    continue;
                }

                write_json_line(
                    &mut file,
                    &json!({
                        "timestamp": event_timestamp(message.timestamp, updated_at),
                        "type": "response_item",
                        "payload": {
                            "type": "message",
                            "role": message.role,
                            "content": blocks,
                        }
                    }),
                )?;
            }
            SessionEvent::Reasoning(reasoning) => {
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
            }
            SessionEvent::ToolCall(call) => {
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
            }
            SessionEvent::ToolResult(result) => {
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
            }
        }
    }

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

fn codex_session_id(candidate: &str) -> String {
    if Uuid::parse_str(candidate).is_ok() {
        candidate.to_string()
    } else {
        Uuid::now_v7().to_string()
    }
}

fn format_name(format: SessionFormat) -> &'static str {
    match format {
        SessionFormat::Ir => "IR",
        SessionFormat::Codex => "Codex",
        SessionFormat::Claude => "Claude",
    }
}
