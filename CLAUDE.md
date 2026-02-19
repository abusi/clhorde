# clhorde

A TUI tool for orchestrating multiple Claude Code CLI instances in parallel. Built with Rust, ratatui, and crossterm.

## What it does

clhorde lets you queue up multiple prompts and run them concurrently across a pool of `claude` CLI workers. Interactive workers run in a real PTY with the full Claude Code TUI embedded via `alacritty_terminal`, while one-shot workers use `stream-json` for lightweight text streaming.

## Architecture

```
src/
├── main.rs       # Entry point, terminal setup, event loop (crossterm + tokio::select!)
├── app.rs        # App state, mode handling, keybindings (vim-style: Normal/Insert/View/Interact/PtyInteract/Filter)
├── prompt.rs     # Prompt data model (id, text, status, output, timing, pty_state)
├── ui.rs         # ratatui rendering (status bar, prompt list, output viewer, PTY grid renderer, input bar, help bar)
├── worker.rs     # Worker dispatch (routes interactive→PTY, one-shot→stream-json)
└── pty_worker.rs # PTY worker lifecycle (portable-pty spawn, alacritty_terminal grid, key encoding, resize)
```

### Key design decisions

- **Event handling**: Crossterm events are read on a dedicated OS thread (not async) and forwarded via `mpsc` channel to avoid blocking the tokio runtime.
- **Worker threads**: Each `claude` subprocess runs in a std::thread (not tokio task) with separate reader/writer threads for stdout parsing and stdin writing.
- **Communication**: Workers send `WorkerMessage` variants (OutputChunk, PtyUpdate, Finished, SpawnError) back to the app via `tokio::sync::mpsc`. The app sends `WorkerInput` (SendInput, SendBytes, Kill) to workers.
- **Dual architecture (PTY + stream-json)**: Interactive workers run in a real PTY via `portable-pty`, with the full Claude Code TUI rendered through `alacritty_terminal`. One-shot workers use the lighter `stream-json` protocol for text-only output. This hybrid gives interactive prompts the full Claude experience (tool use visibility, permission prompts, rich formatting) while keeping one-shot prompts lightweight.
- **PTY terminal emulation**: The `alacritty_terminal` crate provides a headless terminal emulator. PTY output bytes are fed to `Processor::advance()` which updates a `Term` grid. The UI reads this grid each frame, mapping alacritty cell colors/flags to ratatui styles.
- **Claude CLI integration**: Two spawn strategies based on prompt mode:
  - **Interactive (PTY)**: `claude "prompt" --dangerously-skip-permissions` — runs in a real PTY, full TUI embedded in the right panel. Keystrokes forwarded in PtyInteract mode.
  - **One-shot**: `claude -p "prompt" --output-format stream-json --verbose --include-partial-messages --dangerously-skip-permissions` — prompt as CLI arg, no stdin writer, process exits after responding.
  - Removes `CLAUDECODE` env var to avoid nesting issues.

## Dependencies

- `ratatui` 0.30 — TUI framework
- `crossterm` 0.28 — terminal backend
- `tokio` 1 (full features) — async runtime
- `serde` 1 — serialization
- `serde_json` 1 — JSON parsing for claude stream protocol
- `toml` 0.8 — config file parsing
- `dirs` 6 — XDG data/config directory resolution
- `chrono` 0.4 — timestamps for export filenames
- `alacritty_terminal` 0.25 — headless terminal emulator for PTY grid rendering
- `portable-pty` 0.9 — cross-platform PTY allocation and subprocess management

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
- `s` — interact with running/idle prompt
- `m` — toggle prompt mode (interactive / one-shot)
- `r` — retry selected completed/failed prompt
- `J`/`K` — move selected pending prompt down/up in queue
- `/` — enter filter mode (search prompts)
- `+`/`-` — increase/decrease max workers (1–20)
- `q` — quit (with confirmation if workers active)

### Insert mode
- `Enter` — submit prompt
- `Esc` — cancel
- `Up`/`Down` — cycle through prompt history (when no suggestions visible)
- `Tab` — accept directory or template suggestion
- Type `:name` to expand a template

### View mode
- `j`/`k` — scroll output
- `s` — enter interact mode (send follow-up to running prompt)
- `f` — toggle auto-scroll
- `w` — export output to file (`~/clhorde-output-{id}-{timestamp}.md`)
- `x` — kill running worker
- `Esc`/`q` — back to normal

### Interact mode (one-shot workers)
- `Enter` — send message to running worker
- `Esc` — back to normal

### PTY Interact mode (interactive workers)
- All keystrokes forwarded directly to the PTY
- `Esc` — back to view mode

### Filter mode
- Type to filter prompts (live filtering, case-insensitive)
- `Enter` — apply filter and return to normal
- `Esc` — clear filter and return to normal

## Config files

- `~/.config/clhorde/keymap.toml` — custom keybindings (see `keymap_example.toml`)
- `~/.config/clhorde/templates.toml` — prompt templates
- `~/.local/share/clhorde/history` — prompt history (auto-managed)

### Templates format

```toml
[templates]
review = "Review this code for bugs and security issues:"
explain = "Explain what this code does:"
refactor = "Refactor this code to be more idiomatic:"
```

Type `:review` in insert mode and press Tab to expand.

## Code conventions

- Rust 2021 edition, MSRV 1.88
- No `main.rs` should contain business logic — it only wires up terminal and event loop
- State mutations go through `App` methods
- UI rendering is stateless (pure function of `App` state) in `ui.rs`
- Worker communication uses typed enums (`WorkerMessage`, `WorkerInput`), not raw strings
