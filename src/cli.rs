use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use serde_json::json;
use uuid::Uuid;

use crate::formats::{self, load_session, materialize, resolve_input};
use crate::ir::{SessionEvent, SessionFormat, SourceFormat, UniversalSession};

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Translate session storage between Codex, Claude, and a universal IR"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Inspect(InspectArgs),
    Import(ImportArgs),
    Export(ExportArgs),
    Convert(ConvertArgs),
}

#[derive(Debug, Args)]
struct InspectArgs {
    input: PathBuf,
    #[arg(long, value_enum, default_value = "auto")]
    from: SourceFormat,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ImportArgs {
    input: PathBuf,
    output: PathBuf,
    #[arg(long, value_enum, default_value = "auto")]
    from: SourceFormat,
}

#[derive(Debug, Args)]
struct ExportArgs {
    input: PathBuf,
    output: PathBuf,
    #[arg(long, value_enum)]
    to: SessionFormat,
    #[arg(long)]
    new_session_id: bool,
}

#[derive(Debug, Args)]
struct ConvertArgs {
    input: PathBuf,
    output: PathBuf,
    #[arg(long, value_enum, default_value = "auto")]
    from: SourceFormat,
    #[arg(long, value_enum)]
    to: SessionFormat,
    #[arg(long)]
    new_session_id: bool,
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Inspect(args) => inspect(args),
        Command::Import(args) => import(args),
        Command::Export(args) => export(args),
        Command::Convert(args) => convert(args),
    }
}

fn inspect(args: InspectArgs) -> Result<()> {
    let detected = resolve_input(&args.input, args.from)?.format;
    let session = load_session(&args.input, args.from)?;
    let summary = summarize(&session);

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "detected_format": detected,
                "summary": summary,
            }))?
        );
    } else {
        println!("format: {}", format_name(detected));
        println!("session_id: {}", session.metadata.session_id);
        if let Some(title) = &session.metadata.title {
            println!("title: {title}");
        }
        if let Some(cwd) = &session.metadata.cwd {
            println!("cwd: {}", cwd.display());
        }
        println!("events: {}", session.events.len());
        for (kind, count) in summary {
            println!("{kind}: {count}");
        }
    }

    Ok(())
}

fn import(args: ImportArgs) -> Result<()> {
    let session = load_session(&args.input, args.from)?;
    formats::write_ir(&session, &args.output)?;
    println!("{}", args.output.display());
    Ok(())
}

fn export(args: ExportArgs) -> Result<()> {
    let mut session = formats::load_ir(&args.input)?;
    maybe_rekey_session(&mut session, args.new_session_id, args.to);
    let path = materialize(&session, args.to, &args.output)?;
    println!("{}", path.display());
    Ok(())
}

fn convert(args: ConvertArgs) -> Result<()> {
    let mut session = load_session(&args.input, args.from)
        .with_context(|| format!("failed to load source session {}", args.input.display()))?;
    maybe_rekey_session(&mut session, args.new_session_id, args.to);
    let path = materialize(&session, args.to, &args.output)?;
    println!("{}", path.display());
    Ok(())
}

fn summarize(session: &UniversalSession) -> BTreeMap<&'static str, usize> {
    let mut counts = BTreeMap::new();
    for event in &session.events {
        let key = match event {
            SessionEvent::Message(_) => "message",
            SessionEvent::Reasoning(_) => "reasoning",
            SessionEvent::ToolCall(_) => "tool_call",
            SessionEvent::ToolResult(_) => "tool_result",
        };
        *counts.entry(key).or_insert(0) += 1;
    }
    counts
}

fn maybe_rekey_session(
    session: &mut UniversalSession,
    new_session_id: bool,
    target: SessionFormat,
) {
    if !new_session_id {
        if target == SessionFormat::Codex && Uuid::parse_str(&session.metadata.session_id).is_err()
        {
            session.metadata.session_id = Uuid::now_v7().to_string();
        }
        if target == SessionFormat::Claude && Uuid::parse_str(&session.metadata.session_id).is_err()
        {
            session.metadata.session_id = Uuid::new_v4().to_string();
        }
        return;
    }

    session.metadata.session_id = match target {
        SessionFormat::Ir => Uuid::new_v4().to_string(),
        SessionFormat::Codex => Uuid::now_v7().to_string(),
        SessionFormat::Claude => Uuid::new_v4().to_string(),
    };
}

fn format_name(format: SessionFormat) -> &'static str {
    match format {
        SessionFormat::Ir => "ir",
        SessionFormat::Codex => "codex",
        SessionFormat::Claude => "claude",
    }
}
