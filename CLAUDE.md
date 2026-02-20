# clhorde

A TUI tool for orchestrating multiple Claude Code CLI instances in parallel. Built with Rust, ratatui, and crossterm.

## What it does

clhorde lets you queue up multiple prompts and run them concurrently across a pool of `claude` CLI workers. Interactive workers run in a real PTY with the full Claude Code TUI embedded via `alacritty_terminal`, while one-shot workers use `stream-json` for lightweight text streaming.

## Architecture

```
src/
├── main.rs         # Entry point, terminal setup, event loop (crossterm + tokio::select!)
├── app.rs          # App state, mode handling, keybindings (vim-style: Normal/Insert/View/Interact/PtyInteract/Filter)
├── prompt.rs       # Prompt data model (id, text, status, output, timing, pty_state, uuid, session_id, worktree)
├── persistence.rs  # Per-prompt file persistence (save/load/prune JSON files)
├── ui.rs           # ratatui rendering (status bar, prompt list, output viewer, PTY grid renderer, input bar, help bar)
├── worker.rs       # Worker dispatch (routes interactive→PTY, one-shot→stream-json, --resume support)
├── pty_worker.rs   # PTY worker lifecycle (portable-pty spawn, alacritty_terminal grid, key encoding, resize)
└── worktree.rs     # Git worktree helpers (create/remove/detect via `git` CLI)
```

### Key design decisions

- **Event handling**: Crossterm events are read on a dedicated OS thread (not async) and forwarded via `mpsc` channel to avoid blocking the tokio runtime.
- **Worker threads**: Each `claude` subprocess runs in a std::thread (not tokio task) with separate reader/writer threads for stdout parsing and stdin writing.
- **Communication**: Workers send `WorkerMessage` variants (OutputChunk, PtyUpdate, Finished, SpawnError, SessionId) back to the app via `tokio::sync::mpsc`. The app sends `WorkerInput` (SendInput, SendBytes, Kill) to workers.
- **Persistence**: Each prompt is persisted as a UUID v7-named JSON file in `~/.local/share/clhorde/prompts/`. On startup, all prompt files are loaded and restored (as Completed/Failed — no auto-dispatch). The `[settings]` section in `keymap.toml` controls `max_saved_prompts` (default: 100) for automatic pruning.
- **Git worktree isolation**: Per-prompt opt-in via `Ctrl+W` in Insert mode. When enabled, `main.rs` creates a detached git worktree (`git worktree add --detach ../<repo>-wt-<id> HEAD`) before spawning the worker, and overrides the worker's `cwd` to the worktree. Cleanup is controlled by the `worktree_cleanup` setting (`"manual"` default keeps worktrees, `"auto"` removes them on worker finish/kill). Worktree operations use `std::process::Command` (synchronous `git` CLI), not `git2`. The `worktree.rs` module provides `create_worktree()`, `remove_worktree()`, `repo_root()`, `repo_name()`, `is_git_repo()`. Worktree paths are stored on `Prompt.worktree_path` and persisted in the JSON file.
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
- `uuid` 1 (v7 feature) — UUID v7 generation for prompt file names

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
- `R` — resume selected completed/failed prompt (uses `--resume` to continue session)
- `J`/`K` — move selected pending prompt down/up in queue
- `/` — enter filter mode (search prompts)
- `+`/`-` — increase/decrease max workers (1–20)
- `q` — quit (with confirmation if workers active)

### Insert mode
- `Enter` — submit prompt
- `Esc` — cancel
- `Up`/`Down` — cycle through prompt history (when no suggestions visible)
- `Tab` — accept directory or template suggestion
- `Ctrl+W` — toggle git worktree isolation for this prompt (shows `[WT]` indicator)
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

- `~/.config/clhorde/keymap.toml` — custom keybindings and settings (see `keymap_example.toml`)
- `~/.config/clhorde/templates.toml` — prompt templates
- `~/.local/share/clhorde/history` — prompt history (auto-managed)
- `~/.local/share/clhorde/prompts/` — per-prompt persistence files (UUID v7 JSON, auto-managed)

### Templates format

```toml
[templates]
review = "Review this code for bugs and security issues:"
explain = "Explain what this code does:"
refactor = "Refactor this code to be more idiomatic:"
```

Type `:review` in insert mode and press Tab to expand.

### Settings

Add a `[settings]` section to `keymap.toml`:

```toml
[settings]
max_saved_prompts = 100    # Maximum prompt files to keep (default: 100)
worktree_cleanup = "manual" # "manual" (default) or "auto" — auto removes worktrees on worker finish
```

## CLI subcommands

### `clhorde store` — manage persisted prompts

```bash
clhorde store list              # List all stored prompts
clhorde store count             # Show counts by state
clhorde store path              # Print storage directory
clhorde store drop all          # Drop all stored prompts
clhorde store drop completed    # Drop completed only
clhorde store drop failed       # Drop failed only
clhorde store drop pending      # Drop pending only
clhorde store keep completed    # Keep completed, drop rest
clhorde store keep failed       # Keep failed, drop rest
clhorde store clean-worktrees   # Remove lingering git worktrees from completed prompts
```

### `clhorde prompt-from-files` — load prompts from files

Reads file contents and queues them as prompts, then launches the TUI. Each file becomes one pending prompt. Shell glob expansion handles patterns. Comma-separated values within a single argument are also split into individual file paths.

All prompts loaded via `prompt-from-files` automatically have **worktree isolation enabled**, so each prompt gets its own git worktree. Use `--run-path <path>` to specify the working directory (and git repo) for all prompts.

```bash
clhorde prompt-from-files tasks/*.md                          # Load all .md files as prompts
clhorde prompt-from-files --run-path /path/to/repo tasks/*.md # Run in a specific directory
clhorde prompt-from-files a.txt b.txt c.txt                   # Load specific files
clhorde prompt-from-files a.txt,b.txt c.txt                   # Comma-separated + space-separated
```

## Code conventions

- Rust 2021 edition, MSRV 1.88
- No `main.rs` should contain business logic — it only wires up terminal and event loop
- State mutations go through `App` methods
- UI rendering is stateless (pure function of `App` state) in `ui.rs`
- Worker communication uses typed enums (`WorkerMessage`, `WorkerInput`), not raw strings
