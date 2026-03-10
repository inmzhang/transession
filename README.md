# transession

`transession` translates interactive session history between Codex and Claude Code.

The default workflow is native-to-native conversion by session id:

```bash
transession --from claude --to codex <SESSION_ID>
transession --from codex --to claude <SESSION_ID>
```

`transession` automatically creates a fresh target session id, writes the converted session into the target tool's local storage, and prints the exact resume command to use next.

## Install

Once the crate is published on crates.io:

```bash
cargo install transession
```

From the repository:

```bash
cargo install --path .
```

From source for local development:

```bash
git clone https://github.com/inmzhang/transession.git
cd transession
cargo build --release
```

## Quick Start

Convert a Claude session into Codex and immediately resume it:

```bash
transession --from claude --to codex <CLAUDE_SESSION_ID>
codex resume <NEW_CODEX_SESSION_ID>
```

Convert a Codex session into Claude and resume it:

```bash
transession --from codex --to claude <CODEX_SESSION_ID>
claude -r <NEW_CLAUDE_SESSION_ID>
```

If you want to write into non-default storage roots, override the target location explicitly:

```bash
transession --from claude --to codex <SESSION_ID> --output ./tmp/codex-home
transession --from codex --to claude <SESSION_ID> --output ./tmp/claude-home
```

## Session Lookup

For Codex and Claude inputs, `transession` accepts either:

- a native session id
- a direct session file path

By default it searches:

- Codex: `TRANSESSION_CODEX_HOME`, then `CODEX_HOME`, then `~/.codex`
- Claude: `TRANSESSION_CLAUDE_HOME`, then `CLAUDE_HOME`, then `~/.claude`

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

Known omissions:

- opaque reasoning payloads and token-accounting side data
- Codex SQLite state and shell snapshot sidecars
- Claude subagent trees and tool-result sidecar directories
- platform-specific runtime caches outside the main session log

## Advanced Usage

There is also a portable intermediate representation for debugging and advanced workflows, but it is not the main interface.

Advanced commands remain available:

```bash
transession inspect <SESSION_ID> --from claude
transession import <SESSION_ID> ./session.json --from codex
transession export ./session.json ./out/codex-home --to codex --new-session-id
transession convert <SESSION_ID> ./out/claude-home --from codex --to claude --new-session-id
```

## AI Disclaimer

This project was built with Codex. The code and documentation were generated and refined collaboratively with AI assistance, then validated locally with tests and CLI smoke checks.
