use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
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

pub fn load(path: &Path) -> Result<UniversalSession> {
    let file = File::open(path)
        .with_context(|| format!("failed to open Claude session {}", path.display()))?;

    let mut session = UniversalSession::new(Uuid::new_v4().to_string());
    session.metadata.source_format = Some(SessionFormat::Claude);

    for line in BufReader::new(file).lines() {
        let line = line.with_context(|| format!("failed to read {}", path.display()))?;
        if line.trim().is_empty() {
            continue;
        }

        let value: Value = serde_json::from_str(&line)
            .with_context(|| format!("invalid JSONL in {}", path.display()))?;
        import_metadata(&mut session.metadata, &value);

        // `isMeta` entries are injected context rather than conversation, and
        // sidechains are subagent transcripts the other platform has no place
        // for.
        let flagged = |key| value.get(key).and_then(Value::as_bool) == Some(true);
        if flagged("isMeta") || flagged("isSidechain") {
            continue;
        }

        match value.get("type").and_then(Value::as_str) {
            Some("user") => import_user_entry(&mut session.events, &value),
            Some("assistant") => import_assistant_entry(&mut session.events, &value),
            _ => {}
        }
    }

    if session.metadata.title.is_none() {
        session.metadata.title = first_user_text(&session);
    }

    Ok(session)
}

fn import_metadata(metadata: &mut SessionMetadata, value: &Value) {
    let field = |key| value.get(key).and_then(Value::as_str).map(str::to_string);

    if let Some(session_id) = field("sessionId") {
        metadata.original_session_id = Some(session_id.clone());
        metadata.session_id = session_id;
        metadata.source_format = Some(SessionFormat::Claude);
    }
    if let Some(cwd) = field("cwd") {
        metadata.cwd = Some(PathBuf::from(cwd));
    }
    if let Some(branch) = field("gitBranch") {
        metadata.git_branch = Some(branch);
    }
    if let Some(version) = field("version") {
        metadata.platform_version = Some(version);
    }
    if let Some(model) = value
        .get("message")
        .and_then(|message| message.get("model"))
        .and_then(Value::as_str)
    {
        metadata.model = Some(model.to_string());
    }
    update_time_bounds(
        metadata,
        field("timestamp").as_deref().and_then(parse_datetime),
    );
}

fn import_user_entry(events: &mut Vec<SessionEvent>, value: &Value) {
    let (id, parent_id, timestamp) = entry_identity(value);
    let Some(content) = value
        .get("message")
        .and_then(|message| message.get("content"))
    else {
        return;
    };

    match content {
        Value::String(text) if !text.trim().is_empty() => push_user_message(
            events,
            vec![ContentBlock::text("text", text.clone())],
            &id,
            &parent_id,
            timestamp,
        ),
        Value::Array(items) => {
            // Claude batches tool results into the following user turn; split
            // them back out so the IR keeps one event per logical step.
            let mut blocks = Vec::new();
            for item in items {
                if item.get("type").and_then(Value::as_str) == Some("tool_result") {
                    push_user_message(
                        events,
                        std::mem::take(&mut blocks),
                        &id,
                        &parent_id,
                        timestamp,
                    );
                    events.push(SessionEvent::ToolResult(ToolResultEvent {
                        id: id.clone(),
                        parent_id: parent_id.clone(),
                        call_id: item
                            .get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        timestamp,
                        output: item.get("content").cloned().unwrap_or(Value::Null),
                        is_error: item.get("is_error").and_then(Value::as_bool) == Some(true),
                        metadata: BTreeMap::new(),
                    }));
                } else {
                    blocks.push(normalize_block(item));
                }
            }
            push_user_message(events, blocks, &id, &parent_id, timestamp);
        }
        _ => {}
    }
}

fn push_user_message(
    events: &mut Vec<SessionEvent>,
    blocks: Vec<ContentBlock>,
    id: &Option<String>,
    parent_id: &Option<String>,
    timestamp: Option<DateTime<Utc>>,
) {
    if blocks.is_empty() {
        return;
    }
    events.push(SessionEvent::Message(MessageEvent {
        id: id.clone(),
        parent_id: parent_id.clone(),
        role: "user".to_string(),
        timestamp,
        blocks,
        metadata: BTreeMap::new(),
    }));
}

fn import_assistant_entry(events: &mut Vec<SessionEvent>, value: &Value) {
    let (id, parent_id, timestamp) = entry_identity(value);
    let Some(message) = value.get("message") else {
        return;
    };
    let Some(content) = message.get("content").and_then(Value::as_array) else {
        return;
    };

    let mut metadata = BTreeMap::new();
    for key in ["model", "stop_reason"] {
        if let Some(value) = message.get(key) {
            metadata.insert(key.to_string(), value.clone());
        }
    }

    // One Claude entry can mix thinking, prose and tool calls; each kind
    // becomes its own IR event, flushing whatever was accumulating before it.
    let mut blocks = Vec::new();
    let mut reasoning = Vec::new();

    for (index, item) in content.iter().enumerate() {
        let suffix = |kind| id.as_ref().map(|id| format!("{id}:{kind}:{index}"));
        match item.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                flush_reasoning(
                    events,
                    &mut reasoning,
                    suffix("reasoning"),
                    &parent_id,
                    timestamp,
                    &metadata,
                );
                flush_message(
                    events,
                    &mut blocks,
                    suffix("msg"),
                    &parent_id,
                    timestamp,
                    &metadata,
                );
                events.push(SessionEvent::ToolCall(ToolCallEvent {
                    id: id.clone(),
                    parent_id: parent_id.clone(),
                    call_id: item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                    timestamp,
                    arguments: item.get("input").cloned().unwrap_or(Value::Null),
                    metadata: item
                        .get("caller")
                        .map(|caller| BTreeMap::from([("caller".to_string(), caller.clone())]))
                        .unwrap_or_default(),
                }));
            }
            Some("thinking") => {
                flush_message(
                    events,
                    &mut blocks,
                    suffix("msg"),
                    &parent_id,
                    timestamp,
                    &metadata,
                );
                if let Some(text) = item.get("thinking").and_then(Value::as_str) {
                    reasoning.push(text.to_string());
                }
            }
            _ => {
                flush_reasoning(
                    events,
                    &mut reasoning,
                    suffix("reasoning"),
                    &parent_id,
                    timestamp,
                    &metadata,
                );
                blocks.push(normalize_block(item));
            }
        }
    }

    let tail = |kind: &str| id.as_ref().map(|id| format!("{id}:{kind}"));
    flush_reasoning(
        events,
        &mut reasoning,
        tail("reasoning"),
        &parent_id,
        timestamp,
        &metadata,
    );
    flush_message(
        events,
        &mut blocks,
        id.clone(),
        &parent_id,
        timestamp,
        &metadata,
    );
}

type EntryIdentity = (Option<String>, Option<String>, Option<DateTime<Utc>>);

fn entry_identity(value: &Value) -> EntryIdentity {
    let field = |key| value.get(key).and_then(Value::as_str).map(str::to_string);
    (
        field("uuid"),
        field("parentUuid"),
        field("timestamp").as_deref().and_then(parse_datetime),
    )
}

fn flush_message(
    events: &mut Vec<SessionEvent>,
    blocks: &mut Vec<ContentBlock>,
    id: Option<String>,
    parent_id: &Option<String>,
    timestamp: Option<DateTime<Utc>>,
    metadata: &BTreeMap<String, Value>,
) {
    if blocks.is_empty() {
        return;
    }

    events.push(SessionEvent::Message(MessageEvent {
        id,
        parent_id: parent_id.clone(),
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
    parent_id: &Option<String>,
    timestamp: Option<DateTime<Utc>>,
    metadata: &BTreeMap<String, Value>,
) {
    if summary.is_empty() {
        return;
    }

    events.push(SessionEvent::Reasoning(ReasoningEvent {
        id,
        parent_id: parent_id.clone(),
        timestamp,
        summary: std::mem::take(summary),
        metadata: metadata.clone(),
    }));
}

pub fn write(session: &UniversalSession, output: &Path) -> Result<PathBuf> {
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
    let version = super::claude_cli_version();

    // A `.jsonl` output is a standalone file; anything else is a Claude home,
    // where sessions live under a slug of their working directory.
    let native_store = output.extension().and_then(|ext| ext.to_str()) != Some("jsonl");
    let session_file = if native_store {
        output
            .join("projects")
            .join(path_to_claude_slug(&cwd))
            .join(format!("{session_id}.jsonl"))
    } else {
        output.to_path_buf()
    };

    if let Some(parent) = session_file.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut file = File::create(&session_file)
        .with_context(|| format!("failed to create Claude session {}", session_file.display()))?;

    // Every record repeats the same session envelope and chains to the previous
    // record's uuid; only the type, message and timestamp differ.
    let entry = |kind: &str, parent: &Option<String>, uuid: &str, timestamp, message: Value| {
        json!({
            "parentUuid": parent,
            "isSidechain": false,
            "userType": "external",
            "entrypoint": "cli",
            "cwd": cwd,
            "sessionId": session_id,
            "version": version,
            "gitBranch": git_branch,
            "type": kind,
            "message": message,
            "uuid": uuid,
            "timestamp": rfc3339(event_time(timestamp)),
        })
    };

    let mut previous_uuid: Option<String> = None;
    let mut tool_call_uuids = BTreeMap::new();

    for event in &session.events {
        let uuid = Uuid::new_v4().to_string();
        let line = match event {
            SessionEvent::Message(message) => {
                let (role, blocks) = project_message_for_claude(message);
                let content = encode_message_blocks(&blocks);
                if content.is_null() {
                    continue;
                }
                if role == "assistant" {
                    entry(
                        role,
                        &previous_uuid,
                        &uuid,
                        message.timestamp,
                        assistant_message(content, Value::Null),
                    )
                } else {
                    let mut line = entry(
                        role,
                        &previous_uuid,
                        &uuid,
                        message.timestamp,
                        json!({ "role": "user", "content": content }),
                    );
                    line["permissionMode"] = json!("default");
                    line
                }
            }
            SessionEvent::Reasoning(reasoning) => {
                let content = reasoning
                    .summary
                    .iter()
                    .map(|text| json!({ "type": "thinking", "thinking": text }))
                    .collect::<Vec<_>>();
                entry(
                    "assistant",
                    &previous_uuid,
                    &uuid,
                    reasoning.timestamp,
                    assistant_message(Value::Array(content), Value::Null),
                )
            }
            SessionEvent::ToolCall(call) => {
                tool_call_uuids.insert(call.call_id.clone(), uuid.clone());
                entry(
                    "assistant",
                    &previous_uuid,
                    &uuid,
                    call.timestamp,
                    assistant_message(
                        json!([{
                            "type": "tool_use",
                            "id": call.call_id,
                            "name": call.name,
                            "input": encode_tool_input(&call.arguments),
                            "caller": { "type": "direct" },
                        }]),
                        json!("tool_use"),
                    ),
                )
            }
            SessionEvent::ToolResult(result) => {
                let mut line = entry(
                    "user",
                    &previous_uuid,
                    &uuid,
                    result.timestamp,
                    json!({
                        "role": "user",
                        "content": [{
                            "type": "tool_result",
                            "tool_use_id": result.call_id,
                            "content": encode_tool_result_output(&result.output),
                            "is_error": result.is_error,
                        }]
                    }),
                );
                line["toolUseResult"] = tool_result_summary(&result.output, result.is_error);
                line["sourceToolAssistantUUID"] = json!(tool_call_uuids.get(&result.call_id));
                line
            }
        };

        write_json_line(&mut file, &line)?;
        previous_uuid = Some(uuid);
    }

    if native_store {
        let history_path = output.join("history.jsonl");
        let mut history = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&history_path)
            .with_context(|| format!("failed to open {}", history_path.display()))?;
        write_json_line(
            &mut history,
            &json!({
                "display": derive_title(session).unwrap_or_else(|| "Imported session".to_string()),
                "pastedContents": {},
                "timestamp": event_time(session.metadata.created_at).timestamp_millis(),
                "project": cwd.display().to_string(),
                "sessionId": session_id,
            }),
        )?;
    }

    Ok(session_file)
}

fn event_time(timestamp: Option<DateTime<Utc>>) -> DateTime<Utc> {
    timestamp.unwrap_or_else(Utc::now)
}

/// Claude keys its project directories by the working directory with every
/// non-alphanumeric character replaced by a dash.
fn path_to_claude_slug(path: &Path) -> String {
    let slug: String = path
        .to_string_lossy()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
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

    Value::Array(
        blocks
            .iter()
            .map(|block| {
                if block.kind == "input_image"
                    && let Some(image_url) = block
                        .data
                        .as_ref()
                        .and_then(|data| data.get("image_url"))
                        .and_then(Value::as_str)
                {
                    return encode_claude_image(image_url);
                }

                let mut object = Map::new();
                object.insert("type".to_string(), claude_block_kind(&block.kind).into());
                if let Some(text) = &block.text {
                    let key = if block.kind == "thinking" {
                        "thinking"
                    } else {
                        "text"
                    };
                    object.insert(key.to_string(), text.clone().into());
                }
                match &block.data {
                    Some(Value::Object(extra)) => object.extend(extra.clone()),
                    Some(data) => {
                        object.insert("data".to_string(), data.clone());
                    }
                    None => {}
                }
                Value::Object(object)
            })
            .collect(),
    )
}

fn assistant_message(content: Value, stop_reason: Value) -> Value {
    json!({
        "id": format!("msg_{}", Uuid::new_v4().simple()),
        "type": "message",
        "role": "assistant",
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
    })
}

fn encode_tool_input(input: &Value) -> Value {
    match input {
        Value::Object(_) => input.clone(),
        input => json!({ "input": input }),
    }
}

/// Claude only accepts `text`, `image` and `document` blocks inside a tool
/// result; anything else is flattened to a JSON string.
fn encode_tool_result_output(output: &Value) -> Value {
    let Value::Array(items) = output else {
        return match output {
            Value::String(text) => Value::String(text.clone()),
            other => Value::String(json_to_string(other)),
        };
    };
    if items.is_empty() {
        return Value::String(json_to_string(output));
    }

    let mut encoded = Vec::with_capacity(items.len());
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("input_text" | "output_text") => encoded.push(json!({
                "type": "text",
                "text": item.get("text").and_then(Value::as_str).unwrap_or_default(),
            })),
            Some("input_image") => match item.get("image_url").and_then(Value::as_str) {
                Some(image_url) => encoded.push(encode_claude_image(image_url)),
                None => return Value::String(json_to_string(output)),
            },
            Some("text" | "image" | "document") => encoded.push(item.clone()),
            _ => return Value::String(json_to_string(output)),
        }
    }
    Value::Array(encoded)
}

fn encode_claude_image(image_url: &str) -> Value {
    if let Some(data_url) = image_url.strip_prefix("data:")
        && let Some((media_type, data)) = data_url.split_once(";base64,")
    {
        return json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media_type, "data": data },
        });
    }

    json!({
        "type": "image",
        "source": { "type": "url", "url": image_url },
    })
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
        other => json!({ "value": other }),
    }
}

fn claude_session_id(candidate: &str) -> String {
    Uuid::parse_str(candidate)
        .unwrap_or_else(|_| Uuid::new_v4())
        .to_string()
}

/// Claude only knows `user` and `assistant`. Codex's `developer` role (repo
/// instructions) is kept as a labelled user message rather than dropped.
fn project_message_for_claude(message: &MessageEvent) -> (&'static str, Vec<ContentBlock>) {
    let mut blocks = message.blocks.clone();
    match message.role.as_str() {
        "assistant" => ("assistant", blocks),
        "user" => ("user", blocks),
        other => {
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
        "image" => "image",
        "tool_use" => "tool_use",
        "tool_result" => "tool_result",
        _ => "text",
    }
}
