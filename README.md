# transession

`transession` translates interactive session history between Codex and Claude Code.

The default workflow is direct native-to-native conversion by session id:

```bash
transession --from claude --to codex <SESSION_ID>
transession --from codex --to claude <SESSION_ID>
```

By default, `transession`:

- resolves the source session id from the local Claude or Codex store
- creates a fresh target session id automatically
- writes the translated session into the target tool's storage
- immediately opens the translated session in the target agent

If you only want the translation and do not want to start the target agent yet:

```bash
transession --from claude --to codex <SESSION_ID> --no-open
transession --from codex --to claude <SESSION_ID> --no-open
```

## Install

```bash
cargo install transession
```

Or install directly from GitHub:

```bash
cargo install --git https://github.com/inmzhang/transession.git
```

Or from a local checkout:

```bash
cargo install --path .
```

For development:

```bash
git clone https://github.com/inmzhang/transession.git
cd transession
cargo build --release
```

## Quick Start

Convert a Claude session into Codex and open the translated Codex session immediately:

```bash
transession --from claude --to codex <CLAUDE_SESSION_ID>
```

Convert a Codex session into Claude and open the translated Claude session immediately:

```bash
transession --from codex --to claude <CODEX_SESSION_ID>
```

If you want the translated session to be written somewhere else first, override the target root explicitly:

```bash
transession --from claude --to codex <SESSION_ID> --output ./tmp/codex-home
transession --from codex --to claude <SESSION_ID> --output ./tmp/claude-home
```

When opening after translation, `transession` launches the target CLI with the translated session id. For custom output roots, it sets `CODEX_HOME` for Codex and `CLAUDE_CONFIG_DIR` plus `CLAUDE_HOME` for Claude.

For Codex custom output roots, `transession` also links the installed `auth.json` into the target home when needed so the launched Codex process can authenticate immediately.

## Session Lookup

For Codex and Claude inputs, `transession` accepts either:

- a native session id
- a direct session file path

By default it searches:

- Codex: `TRANSESSION_CODEX_HOME`, then `CODEX_HOME`, then `~/.codex`
- Claude: `TRANSESSION_CLAUDE_HOME`, then `CLAUDE_CONFIG_DIR`, then `CLAUDE_HOME`, then `~/.claude`

That means you can usually use the same id you would pass to `codex resume` or `claude -r`.

## What Gets Preserved

`transession` preserves the main conversation state needed for practical handoff:

- user and assistant messages
- reasoning summaries
- tool calls and tool results
- timestamps
- working directory and branch hints
- lightweight platform metadata needed for native session discovery

## Caveats

`transession` intentionally focuses on the durable conversation logs and lightweight resume metadata. It does not recreate every platform-specific side channel.

The current test suite covers the main happy paths, but real-world session logs are messy and platform behavior keeps evolving. You should expect some edge cases and translation failures to surface over time, and the converter will likely need further iteration as those cases are discovered.

Known omissions:

- opaque reasoning payloads and token-accounting side data
- Codex SQLite state and shell snapshot sidecars
- Claude subagent trees and tool-result sidecar directories
- platform-specific runtime caches outside the main session log

## Development

Local development checks:

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Pre-commit hooks are configured in `.pre-commit-config.yaml`.

To enable them locally:

```bash
pipx install pre-commit
pre-commit install
```

The configured hooks run:

- `cargo fmt --all --check`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test`

GitHub Actions workflows are included:

- `.github/workflows/ci.yml` for formatting, linting, and tests
- `.github/workflows/publish.yml` for dry-run or real crates.io publishing

## Publishing

The repository is prepared for `cargo install transession` once the crate is published.

What you need to do before the real publish:

1. Create a crates.io API token with publish permission.
2. Add that token to the GitHub repository secrets as `CARGO_REGISTRY_TOKEN`.
3. Make sure the version in `Cargo.toml` is the version you want to release.
4. Push the release commit to `master`.

How to run the publish workflow:

- For a dry run in GitHub Actions: open the `publish` workflow and run `workflow_dispatch` with `dry_run=true`.
- For a real publish in GitHub Actions: run `workflow_dispatch` with `dry_run=false`, or push a tag like `v0.1.2`.

The publish workflow will:

- verify formatting
- run clippy with `-D warnings`
- run tests
- verify that a pushed `vX.Y.Z` tag matches the `Cargo.toml` version
- run `cargo publish --locked`

The crate name `transession` appeared available during the latest local check, and `cargo publish --dry-run` succeeded locally. You should still treat name availability as time-sensitive until the first real publish completes.

## Advanced Usage

There is also a portable intermediate representation for debugging and advanced workflows, but it is intentionally not the main interface.

Advanced commands remain available:

```bash
transession inspect <SESSION_ID> --from claude
transession import <SESSION_ID> ./session.json --from codex
transession export ./session.json ./out/codex-home --to codex --new-session-id
transession convert <SESSION_ID> ./out/claude-home --from codex --to claude --new-session-id
```

## AI Disclaimer

This project was built with Codex. The code and documentation were generated and refined collaboratively with AI assistance, then validated locally with tests and CLI smoke checks.
