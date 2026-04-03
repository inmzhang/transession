use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};

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
    after_help = "Quick usage:\n  transession --from claude --to codex <SESSION_ID>\n  transession --from codex --to claude <SESSION_ID>\n  transession --from claude --to codex <SESSION_ID> --no-open\n\nAdvanced usage remains available through subcommands such as inspect/import/export/convert."
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

    #[arg(long)]
    no_open: bool,

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
    let wrote_standalone_jsonl = output.extension().and_then(|ext| ext.to_str()) == Some("jsonl");

    maybe_rekey_session(
        &mut session,
        !cli.keep_session_id && to != SessionFormat::Ir,
        to,
    );
    let path = materialize(&session, to, &output)?;

    println!(
        "created {} session: {}",
        format_name(to),
        session.metadata.session_id
    );
    println!("stored at: {}", path.display());
    if let Some(hint) = resume_hint(to, &session.metadata.session_id) {
        println!("resume with: {hint}");
    }
    maybe_open_session(
        to,
        &session.metadata.session_id,
        &output,
        session.metadata.cwd.as_deref(),
        wrote_standalone_jsonl,
        cli.no_open,
    )?;
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

fn maybe_open_session(
    format: SessionFormat,
    session_id: &str,
    output_root: &std::path::Path,
    session_cwd: Option<&std::path::Path>,
    wrote_standalone_jsonl: bool,
    no_open: bool,
) -> Result<()> {
    if no_open || format == SessionFormat::Ir {
        return Ok(());
    }

    if wrote_standalone_jsonl {
        bail!(
            "automatic open requires writing into a native Codex/Claude home directory, not a standalone .jsonl file; pass --no-open to keep the conversion only"
        );
    }

    let mut command = resume_command(format, session_id, output_root, session_cwd)?;
    println!("opening {} session...", format_name(format));
    std::io::stdout()
        .flush()
        .context("failed to flush stdout")?;

    let status = command
        .status()
        .with_context(|| format!("failed to launch {}", format_name(format)))?;
    if !status.success() {
        bail!(
            "{} exited with status {}",
            format_name(format),
            status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".to_string())
        );
    }

    Ok(())
}

fn resume_command(
    format: SessionFormat,
    session_id: &str,
    output_root: &std::path::Path,
    session_cwd: Option<&std::path::Path>,
) -> Result<ProcessCommand> {
    prepare_runtime_home(format, output_root)?;

    let mut command = match format {
        SessionFormat::Codex => {
            let mut cmd = ProcessCommand::new(codex_binary());
            cmd.arg("resume").arg(session_id);
            cmd
        }
        SessionFormat::Claude => {
            let mut cmd = ProcessCommand::new(claude_binary());
            cmd.arg("-r").arg(session_id);
            cmd
        }
        SessionFormat::Ir => bail!("cannot open IR directly"),
    };

    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    match format {
        SessionFormat::Codex => {
            command.env("CODEX_HOME", output_root);
        }
        SessionFormat::Claude => {
            command.env("CLAUDE_CONFIG_DIR", output_root);
            command.env("CLAUDE_HOME", output_root);
        }
        SessionFormat::Ir => {}
    }

    if let Some(cwd) = session_cwd.filter(|cwd| cwd.is_dir()) {
        command.current_dir(cwd);
    }

    Ok(command)
}

fn codex_binary() -> String {
    std::env::var("TRANSESSION_CODEX_BIN").unwrap_or_else(|_| "codex".to_string())
}

fn claude_binary() -> String {
    std::env::var("TRANSESSION_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string())
}

fn prepare_runtime_home(format: SessionFormat, output_root: &Path) -> Result<()> {
    match format {
        SessionFormat::Codex => bootstrap_codex_auth(output_root),
        SessionFormat::Claude | SessionFormat::Ir => Ok(()),
    }
}

fn bootstrap_codex_auth(output_root: &Path) -> Result<()> {
    let installed_home = installed_codex_home()?;
    if same_path(&installed_home, output_root) {
        return Ok(());
    }

    let source_auth = installed_home.join("auth.json");
    if !source_auth.is_file() {
        return Ok(());
    }

    let target_auth = output_root.join("auth.json");
    if target_auth.exists() {
        return Ok(());
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&source_auth, &target_auth).with_context(|| {
            format!(
                "failed to link Codex auth from {} to {}",
                source_auth.display(),
                target_auth.display()
            )
        })?;
    }

    #[cfg(not(unix))]
    {
        fs::copy(&source_auth, &target_auth).with_context(|| {
            format!(
                "failed to copy Codex auth from {} to {}",
                source_auth.display(),
                target_auth.display()
            )
        })?;
    }

    Ok(())
}

fn installed_codex_home() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(path));
    }

    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".codex"))
}

fn same_path(lhs: &Path, rhs: &Path) -> bool {
    if lhs == rhs {
        return true;
    }

    match (fs::canonicalize(lhs), fs::canonicalize(rhs)) {
        (Ok(lhs), Ok(rhs)) => lhs == rhs,
        _ => false,
    }
}
