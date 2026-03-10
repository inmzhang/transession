use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum SessionFormat {
    Ir,
    Codex,
    Claude,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum SourceFormat {
    Auto,
    Ir,
    Codex,
    Claude,
}

impl SourceFormat {
    pub fn explicit(self) -> Option<SessionFormat> {
        match self {
            Self::Auto => None,
            Self::Ir => Some(SessionFormat::Ir),
            Self::Codex => Some(SessionFormat::Codex),
            Self::Claude => Some(SessionFormat::Claude),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct UniversalSession {
    pub ir_version: String,
    pub metadata: SessionMetadata,
    pub events: Vec<SessionEvent>,
}

impl UniversalSession {
    pub const CURRENT_IR_VERSION: &str = "transession/v1";

    pub fn new(session_id: String) -> Self {
        Self {
            ir_version: Self::CURRENT_IR_VERSION.to_string(),
            metadata: SessionMetadata::new(session_id),
            events: Vec::new(),
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct SessionMetadata {
    pub session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_format: Option<SessionFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

impl SessionMetadata {
    pub fn new(session_id: String) -> Self {
        Self {
            session_id,
            ..Self::default()
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    Message(MessageEvent),
    Reasoning(ReasoningEvent),
    ToolCall(ToolCallEvent),
    ToolResult(ToolResultEvent),
}

impl SessionEvent {
    pub fn timestamp(&self) -> Option<DateTime<Utc>> {
        match self {
            Self::Message(event) => event.timestamp,
            Self::Reasoning(event) => event.timestamp,
            Self::ToolCall(event) => event.timestamp,
            Self::ToolResult(event) => event.timestamp,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct MessageEvent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    pub blocks: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ContentBlock {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ContentBlock {
    pub fn text(kind: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            text: Some(text.into()),
            data: None,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ReasoningEvent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    pub summary: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ToolCallEvent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub call_id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    pub arguments: Value,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ToolResultEvent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub call_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    pub output: Value,
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, Value>,
}
