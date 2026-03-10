# transession

`transession` is a Rust CLI for moving interactive session history between:

- Codex session storage
- Claude Code session storage
- A universal JSON IR (`transession/v1`)

The goal is pragmatic interoperability: preserve user and assistant messages, tool calls, tool results, timestamps, and key workspace metadata well enough to resume work in the other tool without manually copy-pasting a long transcript.

## Status

This first implementation targets the durable JSONL session logs. It does **not** attempt to reproduce every platform-specific side channel such as token counters, opaque encrypted reasoning payloads, SQLite caches, or tool-output sidecar files that are not required for the main conversation history.

## Commands

Inspect a session file:

```bash
cargo run -- inspect ~/.claude/projects/-home-inm-open-source-project-qrippy/d89e26cd-11f2-47e8-bea5-a73ad5458483.jsonl
```

Normalize a native session into the IR:

```bash
cargo run -- import ~/.claude/projects/-home-inm-open-source-project-qrippy/d89e26cd-11f2-47e8-bea5-a73ad5458483.jsonl ./qrippy-session.json
```

Export IR into a target platform:

```bash
cargo run -- export ./qrippy-session.json ~/.codex --to codex --new-session-id
```

Convert directly between native formats:

```bash
cargo run -- convert ~/.claude/projects/-home-inm-open-source-project-qrippy/d89e26cd-11f2-47e8-bea5-a73ad5458483.jsonl ~/.codex --to codex --new-session-id
cargo run -- convert ~/.codex/sessions/2026/03/10/rollout-2026-03-10T15-54-00-019cd6bd-10df-7e61-8506-e9ac5bdf4e6e.jsonl ~/.claude --to claude --new-session-id
```

## Output Modes

When `--output` ends in `.jsonl`, `transession` writes a single session file.

When exporting to a directory:

- `codex` writes a canonical file under `sessions/YYYY/MM/DD/` and appends to `session_index.jsonl`
- `claude` writes a canonical file under `projects/<cwd-slug>/`

## IR Shape

The universal IR is a readable JSON document with:

- session metadata: ids, cwd, branch, timestamps, model hints
- ordered events:
  - `message`
  - `reasoning`
  - `tool_call`
  - `tool_result`

This keeps the core conversation portable without baking in either tool's envelope model.
