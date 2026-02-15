# clhorde

A TUI tool for orchestrating multiple Claude Code CLI instances in parallel. Built with Rust, ratatui, and crossterm.

## What it does

clhorde lets you queue up multiple prompts and run them concurrently across a pool of `claude` CLI workers. Each worker spawns a `claude` subprocess using `stream-json` input/output format, streams results back in real-time, and supports interactive follow-up messages.

## Architecture

```
src/
├── main.rs     # Entry point, terminal setup, event loop (crossterm + tokio::select!)
├── app.rs      # App state, mode handling, keybindings (vim-style: Normal/Insert/View/Interact)
├── prompt.rs   # Prompt data model (id, text, status, output, timing)
├── ui.rs       # ratatui rendering (status bar, prompt list, output viewer, input bar, help bar)
└── worker.rs   # Worker subprocess management (spawns `claude -p --stream-json`, reader/writer threads)
```

### Key design decisions

- **Event handling**: Crossterm events are read on a dedicated OS thread (not async) and forwarded via `mpsc` channel to avoid blocking the tokio runtime.
- **Worker threads**: Each `claude` subprocess runs in a std::thread (not tokio task) with separate reader/writer threads for stdout parsing and stdin writing.
- **Communication**: Workers send `WorkerMessage` variants (OutputChunk, Finished, SpawnError) back to the app via `tokio::sync::mpsc`. The app sends `WorkerInput` (SendInput, Kill) to workers.
- **Claude CLI integration**: Two spawn strategies based on prompt mode:
  - **Interactive**: `claude -p --input-format stream-json --output-format stream-json --verbose --include-partial-messages --dangerously-skip-permissions` — initial prompt sent via stdin JSON, process stays alive for follow-ups.
  - **One-shot**: `claude -p "prompt" --output-format stream-json --verbose --include-partial-messages --dangerously-skip-permissions` — prompt as CLI arg, no stdin writer, process exits after responding.
  - Removes `CLAUDECODE` env var to avoid nesting issues.

## Dependencies

- `ratatui` 0.30 — TUI framework
- `crossterm` 0.28 — terminal backend
- `tokio` 1 (full features) — async runtime
- `serde_json` 1 — JSON parsing for claude stream protocol

## Building and running

```bash
cargo build
cargo run
```

Requires `claude` CLI to be installed and available in PATH.

## Keybindings

### Normal mode
- `i` — enter insert mode (type a prompt)
- `j`/`k` or arrows — navigate prompt list
- `Enter` — view selected prompt output
- `m` — toggle prompt mode (interactive / one-shot)
- `+`/`-` — increase/decrease max workers (1–20)
- `q` — quit

### Insert mode
- `Enter` — submit prompt
- `Esc` — cancel

### View mode
- `j`/`k` — scroll output
- `s` — enter interact mode (send follow-up to running prompt)
- `f` — toggle auto-scroll
- `x` — kill running worker
- `Esc`/`q` — back to normal

### Interact mode
- `Enter` — send message to running worker
- `Esc` — back to view

## Code conventions

- Rust 2021 edition, MSRV 1.88
- No `main.rs` should contain business logic — it only wires up terminal and event loop
- State mutations go through `App` methods
- UI rendering is stateless (pure function of `App` state) in `ui.rs`
- Worker communication uses typed enums (`WorkerMessage`, `WorkerInput`), not raw strings
