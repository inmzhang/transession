# transession

`transession` is a Rust CLI for translating interactive session history between:

- Codex session storage
- Claude Code session storage
- A universal JSON IR (`transession/v1`)

The goal is pragmatic interoperability: preserve messages, reasoning summaries, tool calls, tool results, timestamps, and key workspace metadata well enough to resume work in another tool without manually reconstructing the transcript.

## AI Disclaimer

This project was built with Codex. The code and documentation were generated and refined collaboratively with AI assistance, then validated locally with tests and CLI smoke checks.

## Status

This implementation targets the durable JSON and JSONL session logs and also writes the lightweight discovery files needed for native resume flows such as Codex `session_index.jsonl` and Claude `history.jsonl`.

It still does **not** attempt to reproduce every platform-specific side channel. Notably, it does not recreate:

- opaque reasoning payloads or token accounting side data
- Codex SQLite state or shell snapshot sidecars
- Claude subagent trees or tool-result sidecar directories
- platform-specific runtime caches that are not part of the main conversation log

## Commands

Inspect a session by file path or native session id:

```bash
cargo run -- inspect ./examples/session.json
cargo run -- inspect 123e4567-e89b-12d3-a456-426614174000 --from claude
cargo run -- inspect 019cd69f-2838-76d2-b3d6-ed71ab2bb329 --from codex
```

Normalize a native session into the IR:

```bash
cargo run -- import 123e4567-e89b-12d3-a456-426614174000 ./session.json --from claude
```

Export IR into a target platform:

```bash
cargo run -- export ./session.json ./out/codex-home --to codex --new-session-id
```

Convert directly between native formats:

```bash
cargo run -- convert 123e4567-e89b-12d3-a456-426614174000 ./out/codex-home --to codex --from claude --new-session-id
cargo run -- convert 019cd69f-2838-76d2-b3d6-ed71ab2bb329 ./out/claude-home --to claude --from codex --new-session-id
```

## Native Session Id Lookup

For Codex and Claude inputs, `transession` accepts either:

- a direct session file path
- the native session id used by `codex resume <id>` or `claude -r <id>`

By default it searches:

- Codex: `TRANSESSION_CODEX_HOME`, then `CODEX_HOME`, then `~/.codex`
- Claude: `TRANSESSION_CLAUDE_HOME`, then `CLAUDE_HOME`, then `~/.claude`

This lets you translate straight from a session id without manually locating the backing JSONL file.

## Output Modes

When `--output` ends in `.jsonl`, `transession` writes a single session file.

When exporting to a directory:

- `codex` writes a canonical file under `sessions/YYYY/MM/DD/` and appends to `session_index.jsonl`
- `claude` writes a canonical file under `projects/<cwd-slug>/` and appends to `history.jsonl`

## IR Shape

The universal IR is a readable JSON document with:

- session metadata: ids, cwd, branch, timestamps, model hints, extra platform hints
- ordered events:
  - `message`
  - `reasoning`
  - `tool_call`
  - `tool_result`

This keeps the core conversation portable without baking either platform's envelope model into the canonical representation.
