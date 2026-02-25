# clhorde

A daemon+TUI+CLI system for orchestrating multiple Claude Code CLI instances in parallel. Built with Rust, ratatui, and crossterm.

## What it does

clhorde lets you queue up multiple prompts and run them concurrently across a pool of `claude` CLI workers. A background daemon (`clhorded`) manages the worker pool and prompt state. The TUI (`clhorde`) and CLI (`clhorde-cli`) are thin clients that connect to the daemon via Unix domain sockets. Workers survive TUI restarts, and multiple clients can connect simultaneously.

Interactive workers run in a real PTY with the full Claude Code TUI embedded via `alacritty_terminal`, while one-shot workers use `stream-json` for lightweight text streaming.

## Architecture

The project is a Cargo workspace with 4 crates:

```
crates/
├── clhorde-core/          # Shared library
│   └── src/
│       ├── lib.rs
│       ├── prompt.rs       # Prompt, PromptMode, PromptStatus
│       ├── persistence.rs  # Per-prompt file persistence (UUID v7 JSON)
│       ├── worktree.rs     # Git worktree helpers (create/remove/detect)
│       ├── keymap.rs       # TOML config types, key parsing
│       ├── config.rs       # Path helpers (data_dir, templates, history)
│       ├── protocol.rs     # IPC message types (ClientRequest, DaemonEvent, PromptInfo, DaemonState)
│       └── ipc.rs          # Wire framing, socket path resolution
├── clhorde-daemon/        # Background orchestrator → `clhorded` binary
│   └── src/
│       ├── main.rs         # Daemon entry, PID file, signal handling, socket server
│       ├── orchestrator.rs # Prompt queue, worker dispatch, lifecycle, event broadcast
│       ├── worker.rs       # Worker spawn (PTY or stream-json)
│       ├── pty_worker.rs   # PTY lifecycle, alacritty_terminal grid, byte broadcast
│       ├── ipc_server.rs   # Unix socket accept, per-client framing, dispatch
│       └── session.rs      # Per-client subscription tracking
├── clhorde-tui/           # Terminal UI → `clhorde` binary
│   └── src/
│       ├── main.rs         # Terminal setup, daemon connection, event loop
│       ├── app.rs          # UI-only state (modes, selection, input, scroll)
│       ├── ui.rs           # ratatui rendering
│       ├── editor.rs       # TextBuffer: multi-line cursor-aware input
│       ├── keymap.rs       # Runtime Keymap struct, action enums, build_keymap()
│       ├── ipc_client.rs   # Async daemon connection (send/recv, reconnect)
│       ├── pty_renderer.rs # Local alacritty_terminal Term for PTY byte rendering
│       ├── key_encoding.rs # Crossterm key → PTY byte encoding
│       └── cli.rs          # TUI-specific CLI args (prompt-from-files, --run-path)
└── clhorde-cli/           # Command-line tool → `clhorde-cli` binary
    └── src/
        ├── main.rs         # Arg parsing, subcommand dispatch
        ├── daemon_client.rs # Lightweight async IPC client for one-shot commands
        └── commands/
            ├── mod.rs
            ├── store.rs     # store list/count/drop/keep/clean-worktrees (via daemon)
            ├── submit.rs    # Submit prompts to running daemon
            ├── status.rs    # Query daemon status
            ├── attach.rs    # Attach to prompt and stream output
            ├── qp.rs        # Quick prompt management (local config)
            ├── keys.rs      # Keybinding management (local config)
            └── config.rs    # Config file operations (local config)
```

### Binary names

| Crate | Binary | Purpose |
|-------|--------|---------|
| `clhorde-core` | (library) | Shared types, persistence, config, IPC protocol |
| `clhorde-daemon` | `clhorded` | Background orchestrator, manages workers |
| `clhorde-tui` | `clhorde` | Terminal UI, connects to daemon |
| `clhorde-cli` | `clhorde-cli` | CLI tool for store mgmt + daemon interaction |

### Key design decisions

- **Daemon architecture**: The daemon (`clhorded`) owns all worker state, prompt persistence, and worker dispatch. The TUI and CLI are thin clients that communicate via Unix domain sockets at `~/.local/share/clhorde/daemon.sock`. This enables workers to survive TUI restarts and multiple clients to connect simultaneously.
- **IPC protocol**: Length-delimited frames over Unix domain sockets. JSON for structured messages (`ClientRequest` → daemon, `DaemonEvent` → clients). Binary frames with `0x01` marker for high-throughput PTY byte streaming. Protocol types defined in `clhorde-core::protocol`.
- **PTY byte forwarding**: Daemon owns the PTY (via `portable-pty`), feeds bytes to its local `alacritty_terminal::Term`, and broadcasts raw bytes to subscribed TUI clients. Each TUI maintains its own local `Term` for rendering. 64KB ring buffer per prompt for late-joining clients.
- **Event handling**: Crossterm events are read on a dedicated OS thread (not async) and forwarded via `mpsc` channel to avoid blocking the tokio runtime.
- **Worker threads**: Each `claude` subprocess runs in a std::thread (not tokio task) with separate reader/writer threads for stdout parsing and stdin writing.
- **Communication**: Workers send `WorkerMessage` variants back to the orchestrator. Clients send `ClientRequest` messages. The daemon broadcasts `DaemonEvent` to all subscribed clients.
- **Persistence**: Each prompt is persisted as a UUID v7-named JSON file in `~/.local/share/clhorde/prompts/`. Managed by the daemon. The `[settings]` section in `keymap.toml` controls `max_saved_prompts` (default: 100) for automatic pruning.
- **Git worktree isolation**: Per-prompt opt-in via `Ctrl+W` in Insert mode. The daemon creates a detached git worktree before spawning the worker. Cleanup controlled by `worktree_cleanup` setting.
- **Dual architecture (PTY + stream-json)**: Interactive workers run in a real PTY via `portable-pty`, with the full Claude Code TUI rendered through `alacritty_terminal`. One-shot workers use the lighter `stream-json` protocol for text-only output.
- **Claude CLI integration**: Two spawn strategies based on prompt mode:
  - **Interactive (PTY)**: `claude "prompt" --dangerously-skip-permissions` — runs in a real PTY, full TUI embedded in the right panel. Keystrokes forwarded in PtyInteract mode.
  - **One-shot**: `claude -p "prompt" --output-format stream-json --verbose --include-partial-messages --dangerously-skip-permissions` — prompt as CLI arg, no stdin writer, process exits after responding.
  - Removes `CLAUDECODE` env var to avoid nesting issues.

## Dependencies

### clhorde-core (shared library)
- `serde` 1, `serde_json` 1, `toml` 0.8 — serialization & config
- `crossterm` 0.28 — KeyCode/KeyModifiers types only
- `dirs` 6 — XDG directory resolution
- `uuid` 1 (v7) — prompt file naming
- `chrono` 0.4 — timestamps

### clhorde-daemon
- `clhorde-core`, `tokio` 1, `serde_json` 1
- `alacritty_terminal` 0.25 — headless terminal emulator
- `portable-pty` 0.9 — PTY allocation
- `uuid` 1 — prompt ID generation

### clhorde-tui
- `clhorde-core`, `tokio` 1, `serde_json` 1
- `ratatui` 0.30 — TUI framework
- `crossterm` 0.28 — terminal backend
- `alacritty_terminal` 0.25 — local PTY rendering
- `chrono` 0.4, `dirs` 6

### clhorde-cli
- `clhorde-core`, `tokio` 1, `serde_json` 1
- `crossterm` 0.28 — key types for config commands
- `toml` 0.8, `uuid` 1

## Building and running

```bash
cargo build --release

# Start the daemon (background)
./target/release/clhorded &

# Launch the TUI
./target/release/clhorde

# Or use CLI commands
./target/release/clhorde-cli status
./target/release/clhorde-cli submit "Review the auth module"
```

Requires `claude` CLI to be installed and available in PATH.

The daemon must be running before the TUI or CLI can connect. If the daemon is not running, clients print: `"Failed to connect to daemon. Is it running? Start with: clhorded"`

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
- `~/.local/share/clhorde/daemon.sock` — daemon Unix socket
- `~/.local/share/clhorde/daemon.pid` — daemon PID file

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

## CLI commands

### Daemon commands (require running `clhorded`)

#### `clhorde-cli submit` — submit a prompt

```bash
clhorde-cli submit "Review the auth module"
clhorde-cli submit "Fix login bug" --mode one-shot --worktree
clhorde-cli submit "Explain this code" --cwd /path/to/project
```

Options: `--mode interactive|one-shot` (default: interactive), `--cwd <path>`, `--worktree`

#### `clhorde-cli status` — daemon status

```bash
clhorde-cli status
```

Shows worker count, default mode, and prompt summary table with status, mode, and text.

#### `clhorde-cli attach <id>` — stream prompt output

```bash
clhorde-cli attach 1
```

Streams output/PTY bytes to stdout. For completed prompts, prints full output and exits. For running prompts, streams live output until the worker finishes.

#### `clhorde-cli store` — manage persisted prompts

```bash
clhorde-cli store list              # List all stored prompts
clhorde-cli store count             # Show counts by state
clhorde-cli store path              # Print storage directory
clhorde-cli store drop all          # Drop all stored prompts
clhorde-cli store drop completed    # Drop completed only
clhorde-cli store drop failed       # Drop failed only
clhorde-cli store drop pending      # Drop pending only
clhorde-cli store keep completed    # Keep completed, drop rest
clhorde-cli store keep failed       # Keep failed, drop rest
clhorde-cli store clean-worktrees   # Remove lingering git worktrees from completed prompts
```

### Local commands (no daemon required)

#### `clhorde-cli qp` — quick prompts
#### `clhorde-cli keys` — keybindings
#### `clhorde-cli config` — config file management

### TUI commands

#### `clhorde prompt-from-files` — load prompts from files

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
- No `main.rs` should contain business logic — it only wires up terminal/daemon and event loop
- Orchestrator state mutations go through `Orchestrator` methods in the daemon
- TUI state mutations go through `App` methods, which send `ClientRequest` to the daemon
- UI rendering is stateless (pure function of `App` state) in `ui.rs`
- IPC communication uses typed enums (`ClientRequest`, `DaemonEvent`, `PromptInfo`), not raw strings
- Worker communication uses typed enums (`WorkerMessage`, `WorkerInput`)
