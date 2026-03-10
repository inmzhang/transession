use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde_json::json;
use uuid::Uuid;

use crate::formats::{self, default_output_root, load_session, materialize, resolve_input};
use crate::ir::{SessionEvent, SessionFormat, SourceFormat, UniversalSession};

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Translate session storage between Codex, Claude, and a universal IR",
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true,
    after_help = "Quick usage:\n  transession --from claude --to codex <SESSION_ID>\n  transession --from codex --to claude <SESSION_ID>\n\nAdvanced usage remains available through subcommands such as inspect/import/export/convert."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[arg(long, value_enum)]
    from: Option<SourceFormat>,

    #[arg(long, value_enum)]
    to: Option<SessionFormat>,

    #[arg(long)]
    output: Option<PathBuf>,

    #[arg(long)]
    keep_session_id: bool,

    input: Option<PathBuf>,
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
        Some(Command::Inspect(args)) => inspect(args),
        Some(Command::Import(args)) => import(args),
        Some(Command::Export(args)) => export(args),
        Some(Command::Convert(args)) => convert(args),
        None => quick_convert(cli),
    }
}

fn quick_convert(cli: Cli) -> Result<()> {
    let input = cli.input.context("missing input session id or path")?;
    let from = cli.from.unwrap_or(SourceFormat::Auto);
    let to = cli
        .to
        .context("missing --to; example: transession --from claude --to codex <SESSION_ID>")?;

    let mut session = load_session(&input, from)
        .with_context(|| format!("failed to load source session {}", input.display()))?;

    if to == SessionFormat::Ir && cli.output.is_none() {
        bail!("IR output requires --output with a target file path");
    }

    let output = match cli.output {
        Some(path) => path,
        None => default_output_root(to)?,
    };

    maybe_rekey_session(&mut session, !cli.keep_session_id && to != SessionFormat::Ir, to);
    let path = materialize(&session, to, &output)?;

    println!("created {} session: {}", format_name(to), session.metadata.session_id);
    println!("stored at: {}", path.display());
    if let Some(hint) = resume_hint(to, &session.metadata.session_id) {
        println!("resume with: {hint}");
    }
    Ok(())
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

fn resume_hint(format: SessionFormat, session_id: &str) -> Option<String> {
    match format {
        SessionFormat::Codex => Some(format!("codex resume {session_id}")),
        SessionFormat::Claude => Some(format!("claude -r {session_id}")),
        SessionFormat::Ir => None,
    }
}
