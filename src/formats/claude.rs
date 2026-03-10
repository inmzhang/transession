use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::ir::{
    ContentBlock, MessageEvent, ReasoningEvent, SessionEvent, SessionFormat, SessionMetadata,
    ToolCallEvent, ToolResultEvent, UniversalSession,
};

pub struct ClaudeMaterialization {
    pub session_file: PathBuf,
    pub history_file: Option<PathBuf>,
}

pub fn load(path: &Path) -> Result<UniversalSession> {
    let file = File::open(path)
        .with_context(|| format!("failed to open Claude session {}", path.display()))?;
    let reader = BufReader::new(file);

    let mut session = UniversalSession::new(Uuid::new_v4().to_string());
    session.metadata.source_format = Some(SessionFormat::Claude);

    for line in reader.lines() {
        let line = line.with_context(|| format!("failed to read {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }

        let value: Value = serde_json::from_str(&line)
            .with_context(|| format!("invalid JSONL in {}", path.display()))?;
        import_metadata(&mut session.metadata, &value);

        match value.get("type").and_then(Value::as_str) {
            Some("user") => import_user_entry(&mut session.events, &value),
            Some("assistant") => import_assistant_entry(&mut session.events, &value),
            _ => {}
        }
    }

    if session.metadata.title.is_none() {
        session.metadata.title = derive_title(&session);
    }

    Ok(session)
}

fn import_metadata(metadata: &mut SessionMetadata, value: &Value) {
    if let Some(session_id) = value.get("sessionId").and_then(Value::as_str) {
        metadata.session_id = session_id.to_string();
        metadata.original_session_id = Some(session_id.to_string());
        metadata.source_format = Some(SessionFormat::Claude);
    }
    if let Some(cwd) = value.get("cwd").and_then(Value::as_str) {
        metadata.cwd = Some(PathBuf::from(cwd));
    }
    if let Some(branch) = value.get("gitBranch").and_then(Value::as_str) {
        metadata.git_branch = Some(branch.to_string());
    }
    if let Some(version) = value.get("version").and_then(Value::as_str) {
        metadata.platform_version = Some(version.to_string());
    }
    let timestamp = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_datetime);
    update_time_bounds(metadata, timestamp);
}

fn import_user_entry(events: &mut Vec<SessionEvent>, value: &Value) {
    let timestamp = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_datetime);
    let uuid = value
        .get("uuid")
        .and_then(Value::as_str)
        .map(str::to_string);
    let parent_uuid = value
        .get("parentUuid")
        .and_then(Value::as_str)
        .map(str::to_string);

    let Some(message) = value.get("message") else {
        return;
    };

    let content = message.get("content").cloned().unwrap_or(Value::Null);
    match content {
        Value::String(text) => {
            if text.trim().is_empty() {
                return;
            }
            events.push(SessionEvent::Message(MessageEvent {
                id: uuid,
                parent_id: parent_uuid,
                role: "user".to_string(),
                timestamp,
                blocks: vec![ContentBlock::text("text", text)],
                metadata: BTreeMap::new(),
            }));
        }
        Value::Array(items) => {
            let mut message_blocks = Vec::new();

            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("tool_result") => {
                        if !message_blocks.is_empty() {
                            events.push(SessionEvent::Message(MessageEvent {
                                id: uuid.clone(),
                                parent_id: parent_uuid.clone(),
                                role: "user".to_string(),
                                timestamp,
                                blocks: std::mem::take(&mut message_blocks),
                                metadata: BTreeMap::new(),
                            }));
                        }

                        import_tool_result_block(
                            events,
                            &item,
                            timestamp,
                            uuid.clone(),
                            parent_uuid.clone(),
                        );
                    }
                    _ => message_blocks.push(normalize_block(&item)),
                }
            }

            if !message_blocks.is_empty() {
                events.push(SessionEvent::Message(MessageEvent {
                    id: uuid,
                    parent_id: parent_uuid,
                    role: "user".to_string(),
                    timestamp,
                    blocks: message_blocks,
                    metadata: BTreeMap::new(),
                }));
            }
        }
        _ => {}
    }
}

fn import_assistant_entry(events: &mut Vec<SessionEvent>, value: &Value) {
    let timestamp = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_datetime);
    let uuid = value
        .get("uuid")
        .and_then(Value::as_str)
        .map(str::to_string);
    let parent_uuid = value
        .get("parentUuid")
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(message) = value.get("message") else {
        return;
    };

    let mut shared_metadata = BTreeMap::new();
    if let Some(model) = message.get("model") {
        shared_metadata.insert("model".to_string(), model.clone());
    }
    if let Some(stop_reason) = message.get("stop_reason") {
        shared_metadata.insert("stop_reason".to_string(), stop_reason.clone());
    }

    let content = message.get("content").and_then(Value::as_array).cloned();
    let Some(content) = content else {
        return;
    };

    let mut message_blocks = Vec::new();
    let mut reasoning_blocks = Vec::new();

    for (index, item) in content.into_iter().enumerate() {
        match item.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                flush_reasoning(
                    events,
                    &mut reasoning_blocks,
                    uuid.clone().map(|base| format!("{base}:reasoning:{index}")),
                    parent_uuid.clone(),
                    timestamp,
                    &shared_metadata,
                );
                flush_message(
                    events,
                    &mut message_blocks,
                    uuid.clone().map(|base| format!("{base}:msg:{index}")),
                    parent_uuid.clone(),
                    timestamp,
                    &shared_metadata,
                );

                import_tool_use_block(events, &item, timestamp, uuid.clone(), parent_uuid.clone());
            }
            Some("thinking") => {
                flush_message(
                    events,
                    &mut message_blocks,
                    uuid.clone().map(|base| format!("{base}:msg:{index}")),
                    parent_uuid.clone(),
                    timestamp,
                    &shared_metadata,
                );
                if let Some(text) = item.get("thinking").and_then(Value::as_str) {
                    reasoning_blocks.push(text.to_string());
                }
            }
            _ => {
                flush_reasoning(
                    events,
                    &mut reasoning_blocks,
                    uuid.clone().map(|base| format!("{base}:reasoning:{index}")),
                    parent_uuid.clone(),
                    timestamp,
                    &shared_metadata,
                );
                message_blocks.push(normalize_block(&item));
            }
        }
    }

    flush_reasoning(
        events,
        &mut reasoning_blocks,
        uuid.clone().map(|base| format!("{base}:reasoning")),
        parent_uuid.clone(),
        timestamp,
        &shared_metadata,
    );
    flush_message(
        events,
        &mut message_blocks,
        uuid,
        parent_uuid,
        timestamp,
        &shared_metadata,
    );
}

fn flush_message(
    events: &mut Vec<SessionEvent>,
    blocks: &mut Vec<ContentBlock>,
    id: Option<String>,
    parent_id: Option<String>,
    timestamp: Option<DateTime<Utc>>,
    metadata: &BTreeMap<String, Value>,
) {
    if blocks.is_empty() {
        return;
    }

    events.push(SessionEvent::Message(MessageEvent {
        id,
        parent_id,
        role: "assistant".to_string(),
        timestamp,
        blocks: std::mem::take(blocks),
        metadata: metadata.clone(),
    }));
}

fn flush_reasoning(
    events: &mut Vec<SessionEvent>,
    summary: &mut Vec<String>,
    id: Option<String>,
    parent_id: Option<String>,
    timestamp: Option<DateTime<Utc>>,
    metadata: &BTreeMap<String, Value>,
) {
    if summary.is_empty() {
        return;
    }

    events.push(SessionEvent::Reasoning(ReasoningEvent {
        id,
        parent_id,
        timestamp,
        summary: std::mem::take(summary),
        metadata: metadata.clone(),
    }));
}

fn import_tool_use_block(
    events: &mut Vec<SessionEvent>,
    item: &Value,
    timestamp: Option<DateTime<Utc>>,
    parent_id: Option<String>,
    source_parent: Option<String>,
) {
    let call_id = item
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let name = item
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let arguments = item.get("input").cloned().unwrap_or(Value::Null);

    let mut metadata = BTreeMap::new();
    if let Some(caller) = item.get("caller") {
        metadata.insert("caller".to_string(), caller.clone());
    }

    events.push(SessionEvent::ToolCall(ToolCallEvent {
        id: parent_id,
        parent_id: source_parent,
        call_id,
        name,
        timestamp,
        arguments,
        metadata,
    }));
}

fn import_tool_result_block(
    events: &mut Vec<SessionEvent>,
    item: &Value,
    timestamp: Option<DateTime<Utc>>,
    event_id: Option<String>,
    parent_id: Option<String>,
) {
    let output = item.get("content").cloned().unwrap_or(Value::Null);
    let is_error = item
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    events.push(SessionEvent::ToolResult(ToolResultEvent {
        id: event_id,
        parent_id,
        call_id: item
            .get("tool_use_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        timestamp,
        output,
        is_error,
        metadata: BTreeMap::new(),
    }));
}

pub fn write(session: &UniversalSession, output: &Path) -> Result<PathBuf> {
    let materialization = plan_output(session, output);
    if let Some(parent) = materialization.session_file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let session_id = claude_session_id(&session.metadata.session_id);
    let cwd = session
        .metadata
        .cwd
        .clone()
        .unwrap_or_else(|| PathBuf::from("."));
    let git_branch = session
        .metadata
        .git_branch
        .clone()
        .unwrap_or_else(|| "HEAD".to_string());
    let created_at = session.metadata.created_at.or_else(|| {
        session
            .events
            .iter()
            .filter_map(SessionEvent::timestamp)
            .min()
    });
    let version = if session.metadata.source_format == Some(SessionFormat::Claude) {
        session
            .metadata
            .platform_version
            .clone()
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
    } else {
        env!("CARGO_PKG_VERSION").to_string()
    };

    let mut file = File::create(&materialization.session_file).with_context(|| {
        format!(
            "failed to create Claude session {}",
            materialization.session_file.display()
        )
    })?;

    let mut previous_uuid: Option<String> = None;
    let mut tool_call_to_uuid = BTreeMap::new();

    for event in &session.events {
        match event {
            SessionEvent::Message(message) => {
                let event_uuid = Uuid::new_v4().to_string();
                let (projected_role, projected_blocks) = project_message_for_claude(message);
                let content = encode_message_blocks(&projected_blocks);
                if content.is_null() {
                    continue;
                }

                let line = if projected_role == "assistant" {
                    let assistant_message = claude_assistant_message(content, Value::Null);
                    json!({
                        "parentUuid": previous_uuid,
                        "isSidechain": false,
                        "userType": "external",
                        "cwd": cwd,
                        "sessionId": session_id,
                        "version": version,
                        "gitBranch": git_branch,
                        "message": assistant_message,
                        "type": "assistant",
                        "uuid": event_uuid,
                        "timestamp": event_timestamp(message.timestamp),
                    })
                } else {
                    json!({
                        "parentUuid": previous_uuid,
                        "isSidechain": false,
                        "userType": "external",
                        "cwd": cwd,
                        "sessionId": session_id,
                        "version": version,
                        "gitBranch": git_branch,
                        "type": "user",
                        "message": {
                            "role": "user",
                            "content": content,
                        },
                        "uuid": event_uuid,
                        "timestamp": event_timestamp(message.timestamp),
                        "permissionMode": "default",
                    })
                };

                write_json_line(&mut file, &line)?;
                previous_uuid = line.get("uuid").and_then(Value::as_str).map(str::to_string);
            }
            SessionEvent::Reasoning(reasoning) => {
                let event_uuid = Uuid::new_v4().to_string();
                let content = reasoning
                    .summary
                    .iter()
                    .map(|text| {
                        json!({
                            "type": "thinking",
                            "thinking": text,
                        })
                    })
                    .collect::<Vec<_>>();
                let assistant_message =
                    claude_assistant_message(Value::Array(content), Value::Null);
                let line = json!({
                    "parentUuid": previous_uuid,
                    "isSidechain": false,
                    "userType": "external",
                    "cwd": cwd,
                    "sessionId": session_id,
                    "version": version,
                    "gitBranch": git_branch,
                    "message": assistant_message,
                    "type": "assistant",
                    "uuid": event_uuid,
                    "timestamp": event_timestamp(reasoning.timestamp),
                });
                write_json_line(&mut file, &line)?;
                previous_uuid = line.get("uuid").and_then(Value::as_str).map(str::to_string);
            }
            SessionEvent::ToolCall(call) => {
                let event_uuid = Uuid::new_v4().to_string();
                let assistant_message = claude_assistant_message(
                    json!([{
                        "type": "tool_use",
                        "id": call.call_id,
                        "name": call.name,
                        "input": call.arguments,
                        "caller": { "type": "direct" },
                    }]),
                    Value::String("tool_use".to_string()),
                );
                let line = json!({
                    "parentUuid": previous_uuid,
                    "isSidechain": false,
                    "userType": "external",
                    "cwd": cwd,
                    "sessionId": session_id,
                    "version": version,
                    "gitBranch": git_branch,
                    "message": assistant_message,
                    "type": "assistant",
                    "uuid": event_uuid,
                    "timestamp": event_timestamp(call.timestamp),
                });
                write_json_line(&mut file, &line)?;
                tool_call_to_uuid.insert(call.call_id.clone(), event_uuid.clone());
                previous_uuid = Some(event_uuid);
            }
            SessionEvent::ToolResult(result) => {
                let event_uuid = Uuid::new_v4().to_string();
                let source_uuid = tool_call_to_uuid.get(&result.call_id).cloned();
                let line = json!({
                    "parentUuid": previous_uuid,
                    "isSidechain": false,
                    "userType": "external",
                    "cwd": cwd,
                    "sessionId": session_id,
                    "version": version,
                    "gitBranch": git_branch,
                    "type": "user",
                    "message": {
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": result.call_id,
                            "content": encode_tool_result_output(&result.output),
                            "is_error": result.is_error,
                        }]
                    },
                    "uuid": event_uuid,
                    "timestamp": event_timestamp(result.timestamp),
                    "toolUseResult": tool_result_summary(&result.output, result.is_error),
                    "sourceToolAssistantUUID": source_uuid,
                });
                write_json_line(&mut file, &line)?;
                previous_uuid = Some(event_uuid);
            }
        }
    }

    if let Some(history_file) = &materialization.history_file {
        if let Some(parent) = history_file.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let mut history = OpenOptions::new()
            .create(true)
            .append(true)
            .open(history_file)
            .with_context(|| format!("failed to open {}", history_file.display()))?;
        write_json_line(
            &mut history,
            &json!({
                "display": derive_title(session).unwrap_or_else(|| "Imported session".to_string()),
                "pastedContents": {},
                "timestamp": created_at
                    .unwrap_or_else(Utc::now)
                    .timestamp_millis(),
                "project": cwd.display().to_string(),
                "sessionId": session_id,
            }),
        )?;
    }

    Ok(materialization.session_file)
}

fn plan_output(session: &UniversalSession, output: &Path) -> ClaudeMaterialization {
    if output.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
        return ClaudeMaterialization {
            session_file: output.to_path_buf(),
            history_file: None,
        };
    }

    let cwd = session
        .metadata
        .cwd
        .as_deref()
        .unwrap_or_else(|| Path::new("."));
    let slug = path_to_claude_slug(cwd);
    let session_id = claude_session_id(&session.metadata.session_id);
    ClaudeMaterialization {
        session_file: output
            .join("projects")
            .join(slug)
            .join(format!("{session_id}.jsonl")),
        history_file: Some(output.join("history.jsonl")),
    }
}

fn path_to_claude_slug(path: &Path) -> String {
    let rendered = path.to_string_lossy();
    let mut slug = String::with_capacity(rendered.len());
    for ch in rendered.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
        } else {
            slug.push('-');
        }
    }
    if slug.starts_with('-') {
        slug
    } else {
        format!("-{slug}")
    }
}

fn encode_message_blocks(blocks: &[ContentBlock]) -> Value {
    if blocks.is_empty() {
        return Value::Null;
    }

    let encoded = blocks
        .iter()
        .map(|block| {
            let mut object = Map::new();
            object.insert(
                "type".to_string(),
                Value::String(claude_block_kind(&block.kind).to_string()),
            );
            if let Some(text) = &block.text {
                let text_key = if block.kind == "thinking" {
                    "thinking"
                } else {
                    "text"
                };
                object.insert(text_key.to_string(), Value::String(text.clone()));
            }
            if let Some(data) = &block.data {
                if let Value::Object(extra) = data {
                    object.extend(extra.clone());
                } else {
                    object.insert("data".to_string(), data.clone());
                }
            }
            Value::Object(object)
        })
        .collect::<Vec<_>>();
    Value::Array(encoded)
}

fn claude_assistant_message(content: Value, stop_reason: Value) -> Value {
    let mut message = Map::new();
    message.insert(
        "id".to_string(),
        Value::String(format!("msg_{}", Uuid::new_v4().simple())),
    );
    message.insert("type".to_string(), Value::String("message".to_string()));
    message.insert("role".to_string(), Value::String("assistant".to_string()));
    message.insert("content".to_string(), content);
    message.insert("stop_reason".to_string(), stop_reason);
    message.insert("stop_sequence".to_string(), Value::Null);
    Value::Object(message)
}

fn encode_tool_result_output(output: &Value) -> Value {
    match output {
        Value::Array(_) | Value::Object(_) => output.clone(),
        Value::String(text) => Value::String(text.clone()),
        other => Value::String(other.to_string()),
    }
}

fn tool_result_summary(output: &Value, is_error: bool) -> Value {
    if is_error {
        return Value::String(json_to_string(output));
    }

    match output {
        Value::String(text) => json!({
            "stdout": text,
            "stderr": "",
            "interrupted": false,
            "isImage": false,
            "noOutputExpected": false,
        }),
        other => json!({
            "value": other,
        }),
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

fn event_timestamp(timestamp: Option<DateTime<Utc>>) -> String {
    timestamp
        .unwrap_or_else(Utc::now)
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
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(80)
        .collect()
}

fn claude_session_id(candidate: &str) -> String {
    Uuid::parse_str(candidate)
        .map(|uuid| uuid.to_string())
        .unwrap_or_else(|_| Uuid::new_v4().to_string())
}

fn json_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn project_message_for_claude(message: &MessageEvent) -> (&'static str, Vec<ContentBlock>) {
    match message.role.as_str() {
        "assistant" => ("assistant", message.blocks.clone()),
        "user" => ("user", message.blocks.clone()),
        other => {
            let mut blocks = message.blocks.clone();
            let prefix = format!("[transession imported {other} message]");
            match blocks.first_mut() {
                Some(block) if block.text.is_some() => {
                    let text = block.text.take().unwrap_or_default();
                    block.text = Some(format!("{prefix}\n{text}"));
                }
                _ => blocks.insert(0, ContentBlock::text("text", prefix)),
            }
            ("user", blocks)
        }
    }
}

fn claude_block_kind(kind: &str) -> &'static str {
    match kind {
        "thinking" => "thinking",
        "tool_use" => "tool_use",
        "tool_result" => "tool_result",
        "input_text" | "output_text" | "text" => "text",
        _ => "text",
    }
}
