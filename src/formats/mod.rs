mod claude;
mod codex;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;

use crate::ir::{ContentBlock, SessionEvent, SessionFormat, SessionMetadata, UniversalSession};

#[derive(Debug)]
pub struct ResolvedInput {
    pub path: PathBuf,
    pub format: SessionFormat,
}

pub fn detect_format(path: &Path) -> Result<SessionFormat> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read {} for format detection", path.display()))?;

    // Pretty-printed IR is the only format that spans several lines as one JSON
    // document; everything else is JSONL, where the first line is enough.
    if let Ok(value) = serde_json::from_str::<Value>(&text)
        && value.get("ir_version").is_some()
    {
        return Ok(SessionFormat::Ir);
    }

    let first_line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .context("input file is empty")?;
    let value: Value =
        serde_json::from_str(first_line).context("failed to parse the first JSON line")?;

    if value.get("ir_version").is_some() {
        return Ok(SessionFormat::Ir);
    }
    if value.get("type").and_then(Value::as_str) == Some("session_meta") {
        return Ok(SessionFormat::Codex);
    }
    if value.get("sessionId").is_some() {
        return Ok(SessionFormat::Claude);
    }

    bail!("could not detect format for {}", path.display())
}

/// Accept either a session file path or a native session id, resolving the
/// latter against the local Codex/Claude stores. `format` of `None` means
/// autodetect.
pub fn resolve_input(path: &Path, format: Option<SessionFormat>) -> Result<ResolvedInput> {
    if path.exists() {
        let format = match format {
            Some(format) => format,
            None => detect_format(path)?,
        };
        return Ok(ResolvedInput {
            path: path.to_path_buf(),
            format,
        });
    }

    let session_id = path.to_string_lossy().trim().to_string();
    if session_id.is_empty() {
        bail!("input path is empty");
    }

    match format {
        Some(SessionFormat::Ir) => bail!(
            "IR input must be addressed by file path; session-id lookup only works for Codex and Claude"
        ),
        Some(SessionFormat::Codex) => {
            resolve_codex_session_id(&session_id).map(|path| ResolvedInput {
                path,
                format: SessionFormat::Codex,
            })
        }
        Some(SessionFormat::Claude) => {
            resolve_claude_session_id(&session_id).map(|path| ResolvedInput {
                path,
                format: SessionFormat::Claude,
            })
        }
        None => match (
            resolve_codex_session_id(&session_id).ok(),
            resolve_claude_session_id(&session_id).ok(),
        ) {
            (Some(path), None) => Ok(ResolvedInput {
                path,
                format: SessionFormat::Codex,
            }),
            (None, Some(path)) => Ok(ResolvedInput {
                path,
                format: SessionFormat::Claude,
            }),
            (Some(_), Some(_)) => bail!(
                "session id {session_id} exists in both Codex and Claude stores; specify --from"
            ),
            (None, None) => bail!(
                "could not resolve {session_id} as a path or native session id in the default Codex/Claude stores"
            ),
        },
    }
}

pub fn load_session(path: &Path, format: Option<SessionFormat>) -> Result<UniversalSession> {
    load_resolved(&resolve_input(path, format)?)
}

pub fn load_resolved(input: &ResolvedInput) -> Result<UniversalSession> {
    match input.format {
        SessionFormat::Ir => load_ir(&input.path),
        SessionFormat::Codex => codex::load(&input.path),
        SessionFormat::Claude => claude::load(&input.path),
    }
}

pub fn write_ir(session: &UniversalSession, output: &Path) -> Result<()> {
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("failed to create parent directory for {}", output.display())
        })?;
    }

    let text = serde_json::to_string_pretty(session).context("failed to encode IR JSON")?;
    fs::write(output, text).with_context(|| format!("failed to write {}", output.display()))
}

pub fn load_ir(path: &Path) -> Result<UniversalSession> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("failed to read IR file {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

pub fn materialize(
    session: &UniversalSession,
    target: SessionFormat,
    output: &Path,
) -> Result<PathBuf> {
    match target {
        SessionFormat::Ir => {
            write_ir(session, output)?;
            Ok(output.to_path_buf())
        }
        SessionFormat::Codex => codex::write(session, output),
        SessionFormat::Claude => claude::write(session, output),
    }
}

pub fn default_output_root(target: SessionFormat) -> Result<PathBuf> {
    match target {
        SessionFormat::Codex => codex_root(),
        SessionFormat::Claude => claude_root(),
        SessionFormat::Ir => bail!("IR output requires an explicit file path"),
    }
}

fn resolve_codex_session_id(session_id: &str) -> Result<PathBuf> {
    let sessions_root = codex_root()?.join("sessions");
    let suffix = format!("-{session_id}.jsonl");
    find_in_tree(&sessions_root, |name| name.ends_with(&suffix)).with_context(|| {
        format!(
            "could not find Codex session {session_id} under {}",
            sessions_root.display()
        )
    })
}

fn resolve_claude_session_id(session_id: &str) -> Result<PathBuf> {
    let projects_root = claude_root()?.join("projects");
    let file_name = format!("{session_id}.jsonl");
    find_in_tree(&projects_root, |name| name == file_name).with_context(|| {
        format!(
            "could not find Claude session {session_id} under {}",
            projects_root.display()
        )
    })
}

pub(crate) fn codex_root() -> Result<PathBuf> {
    discover_root("TRANSESSION_CODEX_HOME", &["CODEX_HOME"], ".codex")
}

pub(crate) fn claude_root() -> Result<PathBuf> {
    discover_root(
        "TRANSESSION_CLAUDE_HOME",
        &["CLAUDE_CONFIG_DIR", "CLAUDE_HOME"],
        ".claude",
    )
}

fn discover_root(primary_env: &str, secondary_envs: &[&str], suffix: &str) -> Result<PathBuf> {
    for name in std::iter::once(&primary_env).chain(secondary_envs) {
        if let Some(path) = std::env::var_os(name) {
            return Ok(PathBuf::from(path));
        }
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(suffix))
}

fn find_in_tree(root: &Path, matches: impl Fn(&str) -> bool) -> Result<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(&matches)
            {
                return Ok(path);
            }
        }
    }

    bail!("could not find a matching session under {}", root.display())
}

// ==============================================================================
// Target CLI Versions
// ==============================================================================

// Both stores stamp the writing CLI version into every record, and both CLIs
// use it to decide how to read a session back. Hardcoding a version means the
// translated session claims to come from whatever release we last tested
// against, so we ask the installed CLI instead and keep the constants only as
// a fallback for machines where the other tool is not installed (CI, for
// example).

const CODEX_CLI_VERSION_FALLBACK: &str = "0.147.0";
const CLAUDE_CODE_VERSION_FALLBACK: &str = "2.1.234";

pub(crate) fn codex_binary() -> String {
    std::env::var("TRANSESSION_CODEX_BIN").unwrap_or_else(|_| "codex".to_string())
}

pub(crate) fn claude_binary() -> String {
    std::env::var("TRANSESSION_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string())
}

pub(crate) fn codex_cli_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| detect_cli_version(&codex_binary(), CODEX_CLI_VERSION_FALLBACK))
}

pub(crate) fn claude_cli_version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| detect_cli_version(&claude_binary(), CLAUDE_CODE_VERSION_FALLBACK))
}

fn detect_cli_version(binary: &str, fallback: &str) -> String {
    Command::new(binary)
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| parse_version(&text))
        .unwrap_or_else(|| fallback.to_string())
}

/// Pick the first `x.y.z` token out of a `--version` banner.
///
/// Codex prints `codex-cli 0.147.0` and Claude Code prints
/// `2.1.234 (Claude Code)`, so trimming the decoration off each token and
/// keeping the first numeric triple covers both.
fn parse_version(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|token| token.trim_matches(|ch: char| !ch.is_ascii_digit()))
        .find(|token| {
            let mut parts = token.split('.');
            let numeric = |part: Option<&str>| {
                part.is_some_and(|part| {
                    !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit())
                })
            };
            numeric(parts.next()) && numeric(parts.next()) && numeric(parts.next())
        })
        .map(str::to_string)
}

// ==============================================================================
// Shared JSONL helpers
// ==============================================================================

// Codex and Claude store different schemas but the same shape: one JSON object
// per line, RFC3339 millisecond timestamps, and content blocks that carry their
// text under one of a few well-known keys.

pub(crate) fn write_json_line(file: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *file, value).context("failed to encode JSONL line")?;
    file.write_all(b"\n").context("failed to write newline")
}

pub(crate) fn parse_datetime(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

pub(crate) fn rfc3339(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub(crate) fn update_time_bounds(metadata: &mut SessionMetadata, timestamp: Option<DateTime<Utc>>) {
    let Some(timestamp) = timestamp else {
        return;
    };
    metadata.created_at = Some(
        metadata
            .created_at
            .map_or(timestamp, |at| at.min(timestamp)),
    );
    metadata.updated_at = Some(
        metadata
            .updated_at
            .map_or(timestamp, |at| at.max(timestamp)),
    );
}

/// Split a stored content block into `kind` / `text` / everything else, so an
/// export can put the extras back where they came from.
pub(crate) fn normalize_block(value: &Value) -> ContentBlock {
    const TEXT_KEYS: [&str; 3] = ["text", "thinking", "content"];

    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("text")
        .to_string();
    let text = TEXT_KEYS
        .iter()
        .find_map(|key| value.get(key).and_then(Value::as_str))
        .map(str::to_string);

    let mut object = value.as_object().cloned().unwrap_or_default();
    object.remove("type");
    for key in TEXT_KEYS {
        object.remove(key);
    }
    let data = (!object.is_empty()).then_some(Value::Object(object));

    ContentBlock { kind, text, data }
}

pub(crate) fn json_to_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

/// First non-empty user text, collapsed onto one short line. Both stores show
/// it in their resume picker.
pub(crate) fn first_user_text(session: &UniversalSession) -> Option<String> {
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
            .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
            .find(|text| !text.is_empty())
            .map(|text| text.chars().take(80).collect())
    })
}

pub(crate) fn derive_title(session: &UniversalSession) -> Option<String> {
    session
        .metadata
        .title
        .clone()
        .or_else(|| first_user_text(session))
}

#[cfg(test)]
mod tests {
    use super::parse_version;

    #[test]
    fn parses_installed_cli_banners() {
        assert_eq!(
            parse_version("codex-cli 0.147.0\n").as_deref(),
            Some("0.147.0")
        );
        assert_eq!(
            parse_version("2.1.234 (Claude Code)\n").as_deref(),
            Some("2.1.234")
        );
        assert_eq!(parse_version("some unrelated output"), None);
    }
}
