mod claude;
mod codex;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::ir::{SessionFormat, SourceFormat, UniversalSession};

pub use claude::ClaudeMaterialization;
pub use codex::CodexMaterialization;

#[derive(Debug)]
pub struct ResolvedInput {
    pub path: PathBuf,
    pub format: SessionFormat,
}

pub fn detect_format(path: &Path) -> Result<SessionFormat> {
    let bytes = fs::read(path).with_context(|| {
        format!(
            "failed to read input for format detection: {}",
            path.display()
        )
    })?;
    let text = String::from_utf8(bytes)
        .with_context(|| format!("input is not valid UTF-8: {}", path.display()))?;

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

    if matches!(
        value.get("type").and_then(Value::as_str),
        Some("session_meta")
    ) {
        return Ok(SessionFormat::Codex);
    }

    if value.get("sessionId").is_some() {
        return Ok(SessionFormat::Claude);
    }

    bail!("could not detect format for {}", path.display())
}

pub fn resolve_input(path: &Path, format: SourceFormat) -> Result<ResolvedInput> {
    if path.exists() {
        let resolved_format = match format.explicit() {
            Some(format) => format,
            None => detect_format(path)?,
        };
        return Ok(ResolvedInput {
            path: path.to_path_buf(),
            format: resolved_format,
        });
    }

    let session_id = path.to_string_lossy().trim().to_string();
    if session_id.is_empty() {
        bail!("input path is empty");
    }

    match format.explicit() {
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
        None => {
            let codex = resolve_codex_session_id(&session_id).ok();
            let claude = resolve_claude_session_id(&session_id).ok();
            match (codex, claude) {
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
            }
        }
    }
}

pub fn load_session(path: &Path, format: SourceFormat) -> Result<UniversalSession> {
    let resolved = resolve_input(path, format)?;
    match resolved.format {
        SessionFormat::Ir => load_ir(&resolved.path),
        SessionFormat::Codex => codex::load(&resolved.path),
        SessionFormat::Claude => claude::load(&resolved.path),
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

fn resolve_codex_session_id(session_id: &str) -> Result<PathBuf> {
    let root = codex_root()?;
    let sessions_root = root.join("sessions");
    find_in_tree(&sessions_root, |path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.ends_with(&format!("-{session_id}.jsonl")))
            .unwrap_or(false)
    })
    .with_context(|| {
        format!(
            "could not find Codex session {session_id} under {}",
            sessions_root.display()
        )
    })
}

fn resolve_claude_session_id(session_id: &str) -> Result<PathBuf> {
    let root = claude_root()?;
    let projects_root = root.join("projects");
    find_in_tree(&projects_root, |path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .map(|name| name == format!("{session_id}.jsonl"))
            .unwrap_or(false)
    })
    .with_context(|| {
        format!(
            "could not find Claude session {session_id} under {}",
            projects_root.display()
        )
    })
}

fn codex_root() -> Result<PathBuf> {
    discover_root("TRANSESSION_CODEX_HOME", "CODEX_HOME", ".codex")
}

fn claude_root() -> Result<PathBuf> {
    discover_root("TRANSESSION_CLAUDE_HOME", "CLAUDE_HOME", ".claude")
}

fn discover_root(primary_env: &str, secondary_env: &str, suffix: &str) -> Result<PathBuf> {
    if let Some(path) = env_path(primary_env) {
        return Ok(path);
    }
    if let Some(path) = env_path(secondary_env) {
        return Ok(path);
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(suffix))
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

fn find_in_tree<F>(root: &Path, predicate: F) -> Result<PathBuf>
where
    F: Fn(&Path) -> bool + Copy,
{
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if predicate(&path) {
                return Ok(path);
            }
        }
    }

    bail!("could not find a matching session under {}", root.display())
}
