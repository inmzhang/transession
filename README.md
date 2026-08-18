# transession

`transession` translates interactive session history between Codex and Claude Code.

> [!IMPORTANT]
> **Tested compatibility (2026-08-18):** Codex CLI [`0.147.0`](https://github.com/openai/codex/releases/tag/rust-v0.147.0) and Claude Code [`2.1.234`](https://github.com/anthropics/claude-code/releases/tag/v2.1.234), in both directions, verified by resuming the translated session in the target CLI. If either installed CLI is newer, its private session format is not yet verified against this version of `transession`.

## Install

```bash
cargo install transession
```

Or from GitHub (`--git https://github.com/inmzhang/transession.git`) or a local checkout (`--path .`).

## Usage

```bash
transession --from claude --to codex <SESSION_ID>
transession --from codex --to claude <SESSION_ID>
```

By default `transession` resolves the session id in the local Claude or Codex
store, assigns a fresh target session id, writes the translated session into the
target tool's storage, and immediately opens it in the target agent. Add
`--no-open` to stop after the translation, or `--output <DIR>` to write into a
different store root.

Translated sessions are stamped with the version of the CLI installed on your
machine (`codex --version`, `claude --version`), falling back to the versions
above when the target CLI is not installed.

Sessions can be addressed by native session id or by file path. Store roots are
discovered from, in order:

- Codex: `TRANSESSION_CODEX_HOME`, `CODEX_HOME`, `~/.codex`
- Claude: `TRANSESSION_CLAUDE_HOME`, `CLAUDE_CONFIG_DIR`, `CLAUDE_HOME`, `~/.claude`

`--from` is optional; the format is autodetected when it is omitted.

### Opening the translated session

Only a custom `--output` root redirects the target CLI's home (`CODEX_HOME` for
Codex, `CLAUDE_CONFIG_DIR` plus `CLAUDE_HOME` for Claude), and the installed
credentials are linked into it so the launched CLI can authenticate
immediately. Writing into the installed store leaves those variables alone,
because `CLAUDE_CONFIG_DIR` also moves Claude Code's account file from
`~/.claude.json` to `<dir>/.claude.json` and would otherwise force a fresh
login.

## What Gets Preserved

User and assistant messages, reasoning summaries, tool calls and results,
timestamps, working directory and branch, and the platform metadata needed for
native session discovery.

Not preserved:

- opaque reasoning payloads and token accounting
- Codex's paginated `item_completed` thread items; translated sessions use the legacy event history Codex still replays
- Codex shell snapshot sidecars (the thread row in `state_5.sqlite` is written)
- Claude subagent trees and tool-result sidecar directories
- platform-specific runtime caches outside the main session log

Real-world session logs are messy and both platforms keep evolving, so expect
edge cases beyond the happy paths the test suite covers.

## Advanced Usage

There is also a portable intermediate representation for debugging, reachable
through the subcommands:

```bash
transession inspect <SESSION_ID> --from claude
transession import <SESSION_ID> ./session.json --from codex
transession export ./session.json ./out/codex-home --to codex --new-session-id
transession convert <SESSION_ID> ./out/claude-home --from codex --to claude --new-session-id
```

## Development

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

The same three checks run in `.github/workflows/ci.yml` and, via
`.pre-commit-config.yaml`, as pre-commit hooks (`pipx install pre-commit &&
pre-commit install`).

Releases go through `.github/workflows/publish.yml`: `workflow_dispatch` with
`dry_run=true` for a dry run, or push a `vX.Y.Z` tag matching the `Cargo.toml`
version to publish to crates.io using the `CARGO_REGISTRY_TOKEN` secret.

## AI Disclaimer

This project was built with Codex. The code and documentation were generated and
refined collaboratively with AI assistance, then validated locally with tests
and CLI smoke checks.
