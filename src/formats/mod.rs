mod claude;
mod codex;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::ir::{SessionFormat, SourceFormat, UniversalSession};

pub use claude::ClaudeMaterialization;
pub use codex::CodexMaterialization;

pub fn detect_format(path: &Path) -> Result<SessionFormat> {
    let bytes = fs::read(path).with_context(|| {
        format!(
            "failed to read input for format detection: {}",
            path.display()
        )
    })?;
    let text = String::from_utf8(bytes)
        .with_context(|| format!("input is not valid UTF-8: {}", path.display()))?;
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

pub fn load_session(path: &Path, format: SourceFormat) -> Result<UniversalSession> {
    let resolved = match format.explicit() {
        Some(format) => format,
        None => detect_format(path)?,
    };

    match resolved {
        SessionFormat::Ir => load_ir(path),
        SessionFormat::Codex => codex::load(path),
        SessionFormat::Claude => claude::load(path),
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
