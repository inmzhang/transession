use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde_json::json;
use uuid::Uuid;

use crate::formats::{
    self, default_output_root, load_resolved, load_session, materialize, resolve_input,
};
use crate::ir::{SessionEvent, SessionFormat, UniversalSession};

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

    /// Source format; autodetected when omitted.
    #[arg(long, value_enum)]
    from: Option<SessionFormat>,

    #[arg(long, value_enum)]
    to: Option<SessionFormat>,

    /// Target store root, or a `.jsonl` file for a standalone session.
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
    #[arg(long, value_enum)]
    from: Option<SessionFormat>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ImportArgs {
    input: PathBuf,
    output: PathBuf,
    #[arg(long, value_enum)]
    from: Option<SessionFormat>,
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
    #[arg(long, value_enum)]
    from: Option<SessionFormat>,
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
    let to = cli
        .to
        .context("missing --to; example: transession --from claude --to codex <SESSION_ID>")?;

    let mut session = load_session(&input, cli.from)
        .with_context(|| format!("failed to load source session {}", input.display()))?;

    if to == SessionFormat::Ir && cli.output.is_none() {
        bail!("IR output requires --output with a target file path");
    }

    let output = match cli.output {
        Some(path) => path,
        None => default_output_root(to)?,
    };
    let wrote_standalone_jsonl = output.extension().and_then(|ext| ext.to_str()) == Some("jsonl");

    rekey_session(
        &mut session,
        !cli.keep_session_id && to != SessionFormat::Ir,
        to,
    );
    let path = materialize(&session, to, &output)?;

    println!("created {to} session: {}", session.metadata.session_id);
    println!("stored at: {}", path.display());
    match to {
        SessionFormat::Codex => {
            println!("resume with: codex resume {}", session.metadata.session_id)
        }
        SessionFormat::Claude => println!("resume with: claude -r {}", session.metadata.session_id),
        SessionFormat::Ir => {}
    }

    if !cli.no_open && to != SessionFormat::Ir {
        if wrote_standalone_jsonl {
            bail!(
                "automatic open requires writing into a native Codex/Claude home directory, not a standalone .jsonl file; pass --no-open to keep the conversion only"
            );
        }
        open_session(
            to,
            &session.metadata.session_id,
            &output,
            session.metadata.cwd.as_deref(),
        )?;
    }

    Ok(())
}

fn inspect(args: InspectArgs) -> Result<()> {
    let resolved = resolve_input(&args.input, args.from)?;
    let session = load_resolved(&resolved)?;
    let summary = summarize(&session);

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "detected_format": resolved.format,
                "summary": summary,
            }))?
        );
        return Ok(());
    }

    println!("format: {}", resolved.format);
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
    rekey_session(&mut session, args.new_session_id, args.to);
    println!(
        "{}",
        materialize(&session, args.to, &args.output)?.display()
    );
    Ok(())
}

fn convert(args: ConvertArgs) -> Result<()> {
    let mut session = load_session(&args.input, args.from)
        .with_context(|| format!("failed to load source session {}", args.input.display()))?;
    rekey_session(&mut session, args.new_session_id, args.to);
    println!(
        "{}",
        materialize(&session, args.to, &args.output)?.display()
    );
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

/// Both native stores key their session files by UUID, so a non-UUID id is
/// always replaced. Codex additionally sorts its resume picker by id, which
/// wants the time-ordered v7 flavour.
fn rekey_session(session: &mut UniversalSession, forced: bool, target: SessionFormat) {
    let needs_uuid =
        target != SessionFormat::Ir && Uuid::parse_str(&session.metadata.session_id).is_err();
    if !forced && !needs_uuid {
        return;
    }

    session.metadata.session_id = match target {
        SessionFormat::Codex => Uuid::now_v7(),
        SessionFormat::Ir | SessionFormat::Claude => Uuid::new_v4(),
    }
    .to_string();
}

fn open_session(
    format: SessionFormat,
    session_id: &str,
    output_root: &Path,
    session_cwd: Option<&Path>,
) -> Result<()> {
    let installed_home = match format {
        SessionFormat::Codex => formats::codex_root()?,
        SessionFormat::Claude => formats::claude_root()?,
        SessionFormat::Ir => bail!("IR has no runtime home"),
    };
    let redirected = !same_path(&installed_home, output_root);
    if redirected {
        link_login_state(format, &installed_home, output_root)?;
    }

    let mut command = match format {
        SessionFormat::Codex => {
            let mut command = ProcessCommand::new(formats::codex_binary());
            command.arg("resume").arg(session_id);
            command
        }
        _ => {
            let mut command = ProcessCommand::new(formats::claude_binary());
            command.arg("-r").arg(session_id);
            command
        }
    };
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    // Only redirect the target CLI's home when we wrote somewhere other than
    // its own store. `CLAUDE_CONFIG_DIR` in particular moves the account file
    // from `~/.claude.json` to `<dir>/.claude.json`, so pointing it at the
    // default `~/.claude` would hand Claude Code an empty config and force a
    // fresh login.
    if redirected {
        match format {
            SessionFormat::Codex => {
                command.env("CODEX_HOME", output_root);
            }
            _ => {
                command.env("CLAUDE_CONFIG_DIR", output_root);
                command.env("CLAUDE_HOME", output_root);
            }
        }
    }

    if let Some(cwd) = session_cwd.filter(|cwd| cwd.is_dir()) {
        command.current_dir(cwd);
    }

    println!("opening {format} session...");
    std::io::stdout()
        .flush()
        .context("failed to flush stdout")?;

    let status = command
        .status()
        .with_context(|| format!("failed to launch {format}"))?;
    if !status.success() {
        bail!("{format} exited with {status}");
    }

    Ok(())
}

/// Custom output roots start out empty, so the launched CLI would ask the user
/// to log in again. Link the installed credentials across instead.
///
/// Claude Code needs both files: `.credentials.json` holds the tokens and
/// `.claude.json` holds the account record it checks before onboarding. They
/// are linked rather than copied so a token refresh in the temporary home stays
/// valid in the installed one; the trade-off is that the temporary home also
/// writes its project state back into the installed config.
fn link_login_state(
    format: SessionFormat,
    installed_home: &Path,
    output_root: &Path,
) -> Result<()> {
    let files: &[&str] = match format {
        SessionFormat::Codex => &["auth.json"],
        SessionFormat::Claude => &[".credentials.json", ".claude.json"],
        SessionFormat::Ir => return Ok(()),
    };

    for file in files {
        let source = installed_home.join(file);
        let target = output_root.join(file);
        if !source.is_file() || target.exists() {
            continue;
        }

        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(&source, &target);
        #[cfg(not(unix))]
        let linked = fs::copy(&source, &target).map(|_| ());

        linked.with_context(|| {
            format!(
                "failed to link {} to {}",
                source.display(),
                target.display()
            )
        })?;
    }

    Ok(())
}

fn same_path(lhs: &Path, rhs: &Path) -> bool {
    lhs == rhs
        || matches!(
            (fs::canonicalize(lhs), fs::canonicalize(rhs)),
            (Ok(lhs), Ok(rhs)) if lhs == rhs
        )
}
