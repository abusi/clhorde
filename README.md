# clhorde

A lightweight, single-binary TUI for orchestrating multiple Claude Code CLI instances in parallel. Built with Rust, ratatui, and crossterm.

![Rust](https://img.shields.io/badge/rust-2021-orange)

## Why clhorde?

Tools like [Claude Squad](https://github.com/smtg-ai/claude-squad), [Clark](https://github.com/brianirish/clark), and [VibeMux](https://github.com/UgOrange/vibemux) solve a similar problem—running multiple Claude Code sessions at once. They're great tools, but they all share the same architecture: **tmux sessions + git worktrees**. Each "instance" is a full interactive Claude Code session wrapped in a tmux pane.

clhorde takes a fundamentally different approach:

| | clhorde | tmux-based tools |
|---|---|---|
| **Architecture** | Hybrid: PTY for interactive + stream-json for one-shot | Wraps interactive sessions in tmux panes |
| **Work model** | Prompt queue + worker pool | N independent sessions |
| **Concurrency** | Queue any number of prompts, workers pull from queue | Fixed number of parallel sessions |
| **Dependencies** | Single binary (just needs `claude` in PATH) | Requires tmux, git worktrees |
| **Code isolation** | Each worker is a fresh subprocess—no shared state | Git worktrees per session |
| **Interaction** | Full Claude TUI embedded via PTY + terminal emulator | Full interactive terminal per session |
| **Binary size** | ~1500 lines of Rust | Go/Python projects with larger dependency trees |
| **Runtime** | Native async (tokio) + OS threads | tmux + shell processes |

### The queue model

Most multi-Claude tools give you N parallel sessions and you manually assign work to each one. clhorde works differently: you queue up as many prompts as you want, and a configurable pool of workers (1–20) pulls from the queue automatically. This means you can batch 50 prompts and walk away—workers will chew through them at whatever concurrency you set.

### Direct integration—no tmux

clhorde uses a hybrid approach: interactive workers run in a real PTY with the full Claude Code TUI rendered via an embedded terminal emulator (`alacritty_terminal`). One-shot workers use the lighter `stream-json` protocol for text-only output. No tmux, no screen scraping—just direct subprocess control with proper terminal emulation where it matters.

### Truly zero dependencies (beyond `claude`)

No tmux. No git worktrees. No Python. No Node. Just a single Rust binary and the `claude` CLI. Install and run.

## Features

- **Worker pool with queue** — queue unlimited prompts, configure 1–20 concurrent workers with `+`/`-`
- **Embedded PTY terminal** — interactive workers render the full Claude Code TUI (colors, tool use, formatting) via `alacritty_terminal`
- **Real-time streaming** — output streams token-by-token as Claude generates it
- **Interactive follow-ups** — send messages to running workers mid-conversation, or enter PTY mode for full keyboard control
- **One-shot & interactive modes** — toggle with `m`: one-shot prompts use stream-json, interactive prompts get a full embedded PTY
- **Vim-style modal interface** — Normal, Insert, View, Interact, PtyInteract, and Filter modes
- **Live status dashboard** — active workers, queue depth, completed count, per-prompt elapsed time
- **Auto-scroll** — follows output in real time, toggleable with `f`
- **Kill workers** — terminate a running prompt with `x`
- **Export output** — save a prompt's output to a markdown file with `w`
- **Retry prompts** — re-queue completed or failed prompts with `r`
- **Reorder queue** — move pending prompts up/down with `J`/`K`
- **Search/filter** — press `/` to live-filter prompts by text
- **Prompt history** — `Up`/`Down` in insert mode cycles through previously submitted prompts
- **Session persistence** — auto-saves on quit, restore with `--restore`
- **Prompt templates** — define reusable prompt snippets, expand with `:name` + Tab
- **Quit confirmation** — warns before quitting with active workers
- **Graceful shutdown** — sends EOF to all workers on quit, no orphaned processes

## Install

```bash
git clone https://github.com/your-user/clhorde.git
cd clhorde
cargo build --release
# binary is at target/release/clhorde
```

Requires:
- Rust 1.88+
- `claude` CLI installed and in PATH

## Usage

```bash
clhorde              # fresh session
clhorde --restore    # restore previous session
clhorde --help       # show help
```

That's it. You'll see the TUI. Press `i` to start typing a prompt.

## CLI commands

clhorde includes subcommands for managing configuration without hand-editing TOML files.

### Quick prompts

```bash
clhorde qp list              # list all quick prompts
clhorde qp add g "let's go"  # add a quick prompt on key 'g'
clhorde qp remove g          # remove a quick prompt
```

### Keybindings

```bash
clhorde keys list             # list all keybindings
clhorde keys list normal      # list normal mode only
clhorde keys set normal quit Q          # set quit to Q
clhorde keys set view back Esc q        # set multiple keys
clhorde keys reset normal quit          # reset one action to default
clhorde keys reset normal               # reset entire mode to defaults
```

Valid modes: `normal`, `insert`, `view`, `interact`, `filter`.

### Config file

```bash
clhorde config path           # print config file path
clhorde config init           # create config with all defaults
clhorde config init --force   # overwrite existing config
clhorde config edit           # open config in $EDITOR (or vi)
```

## Keybindings

### Normal mode
| Key | Action |
|-----|--------|
| `i` | Enter insert mode (type a prompt) |
| `j` / `k` / `↑` / `↓` | Navigate prompt list |
| `Enter` | View selected prompt's output |
| `s` | Interact with running/idle prompt |
| `r` | Retry selected completed/failed prompt |
| `J` / `K` | Move selected pending prompt down/up in queue |
| `/` | Enter filter mode (search prompts) |
| `+` / `-` | Increase / decrease max workers (1–20) |
| `m` | Toggle prompt mode (interactive / one-shot) |
| `q` | Quit (confirms if workers active) |

### Insert mode
| Key | Action |
|-----|--------|
| `Enter` | Submit prompt to queue |
| `Esc` | Cancel and return to normal mode |
| `↑` / `↓` | Cycle through prompt history (when no suggestions visible) |
| `Tab` | Accept directory or template suggestion |

### View mode
| Key | Action |
|-----|--------|
| `j` / `k` / `↑` / `↓` | Scroll output |
| `s` | Enter interact mode (send follow-up) |
| `f` | Toggle auto-scroll |
| `w` | Export output to `~/clhorde-output-{id}-{timestamp}.md` |
| `x` | Kill running worker |
| `Esc` / `q` | Back to normal mode |

### Interact mode (one-shot workers)
| Key | Action |
|-----|--------|
| `Enter` | Send follow-up message to worker |
| `Esc` | Back to normal mode |

### PTY Interact mode (interactive workers)
| Key | Action |
|-----|--------|
| *all keys* | Forwarded directly to the PTY |
| `Esc` | Back to view mode |

### Filter mode
| Key | Action |
|-----|--------|
| *type* | Live-filter prompts (case-insensitive) |
| `Enter` | Apply filter and return to normal |
| `Esc` | Clear filter and return to normal |

## Custom keybindings

All keybindings can be remapped via `~/.config/clhorde/keymap.toml` (or `$XDG_CONFIG_HOME/clhorde/keymap.toml`). The config file is optional — missing file or missing keys silently use defaults. You only need to specify what you want to change.

See [`keymap_example.toml`](keymap_example.toml) for the full default keymap with all available actions.

Example — remap quit to `Q` and navigation to arrow keys only:

```toml
[normal]
quit = ["Q"]
select_next = ["Down"]
select_prev = ["Up"]
```

Key names: single characters (`"q"`, `"+"`) or special names (`"Enter"`, `"Esc"`, `"Tab"`, `"Up"`, `"Down"`, `"Left"`, `"Right"`, `"Space"`, `"Backspace"`).

## Quick prompts

Send predefined messages to a running worker with a single keypress in view mode. Add a `[quick_prompts]` section to your `keymap.toml`:

```toml
[quick_prompts]
g = "let's go"
c = "continue"
y = "yes"
n = "no"
```

When viewing a running or idle prompt, pressing `g` immediately sends `"let's go"` to the worker — no need to enter interact mode. The message is echoed in the output panel just like a regular follow-up.

Quick prompt keys must not conflict with view mode bindings (`j`, `k`, `q`, `s`, `f`, `x`, `w`, `Esc`, arrows). If they do, the view binding takes priority.

## Prompt templates

Define reusable prompt snippets in `~/.config/clhorde/templates.toml`:

```toml
[templates]
review = "Review this code for bugs and security issues:"
explain = "Explain what this code does:"
test = "Write unit tests for this code:"
refactor = "Refactor this code to be more idiomatic:"
```

In insert mode, type `:review` and press `Tab` — the template name is replaced with its full text. A suggestion popup shows matching templates as you type.

This is especially useful when combined with working directories. For example, type `/path/to/project: ` then `:review` + Tab to get:

```
/path/to/project: Review this code for bugs and security issues:
```

## Session persistence

clhorde automatically saves your session (all prompts and their outputs) to `~/.local/share/clhorde/session.json` when you quit. To restore a previous session:

```bash
clhorde --restore
```

Completed and failed prompts are restored as-is. Running/idle prompts (whose processes are gone) are restored as completed. Pending prompts are re-queued and will be dispatched to workers.

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│                       main.rs                            │
│  Terminal setup, tokio::select! event loop               │
│  Dispatches queued prompts → worker pool                 │
├─────────────┬───────────────┬──────────────┬─────────────┤
│   app.rs    │    ui.rs      │  worker.rs   │pty_worker.rs│
│  State +    │  Stateless    │  Dispatch +  │ PTY spawn,  │
│  keybinds   │  ratatui +    │  one-shot    │ alacritty   │
│  modes      │  PTY grid     │  stream-json │ term grid   │
├─────────────┴───────────────┴──────────────┴─────────────┤
│                       prompt.rs                          │
│  Data model: id, text, status, output, timing, pty_state │
└──────────────────────────────────────────────────────────┘
```

- **Event handling** — Crossterm events are read on a dedicated OS thread (not async) and forwarded via channel, so the tokio runtime never blocks.
- **Worker threads** — Each `claude` subprocess runs in `std::thread` with separate reader and writer threads for stdout parsing and stdin writing.
- **Dual architecture** — Interactive workers run in a real PTY (`portable-pty`) with terminal emulation (`alacritty_terminal`). One-shot workers use `stream-json` for lightweight text streaming.
- **Communication** — Workers send `WorkerMessage` variants (OutputChunk, PtyUpdate, Finished, SpawnError) back to the app via `tokio::sync::mpsc`. The app sends `WorkerInput` (SendInput, SendBytes, Kill) to workers.
- **Clean shutdown** — PTY workers are terminated by dropping the master PTY (child gets SIGHUP). One-shot workers exit when stdin is closed.

## UI Layout

```
┌──────────────────────────────────────────────────────┐
│ NORMAL | Workers: 2/4 | Queue: 3 | Done: 5 | Total: 10 │
├───────────────────────┬──────────────────────────────┤
│ ▶ ✅ #1 prompt (2.3s) │                              │
│   🔄 #2 prompt (1.1s) │  Selected prompt's output    │
│   ⏳ #3 prompt         │  streams here in real time   │
│   ❌ #4 prompt (0.5s) │                              │
├───────────────────────┴──────────────────────────────┤
│ > type your prompt here_                              │
├──────────────────────────────────────────────────────┤
│ i:insert  q:quit  j/k:navigate  Enter:view  +/-:wkrs │
└──────────────────────────────────────────────────────┘
```

- **Left panel (40%)** — prompt queue with status icons, IDs, and elapsed time
- **Right panel (60%)** — streaming output for the selected prompt
- **Status bar** — mode, worker count, queue depth, completion stats
- **Input bar** — context-aware prompt based on current mode
- **Help bar** — shows available keybindings for current mode

## Prompt modes

clhorde supports two prompt modes, toggled with `m` in Normal mode:

- **Interactive** (default) — spawns `claude "prompt" --dangerously-skip-permissions` in a real PTY. The right panel renders the full Claude Code TUI with colors, tool use, and formatting via `alacritty_terminal`. Press `s` to enter PTY Interact mode and type directly into the Claude session. When the process exits, the terminal output is extracted and stored for export/session persistence.
- **One-shot** — spawns `claude -p "prompt text" --output-format stream-json`. The prompt is passed as a CLI argument. No stdin writer, no follow-ups. The process exits after responding and goes directly to Completed.

The current default mode is shown in the status bar (`[interactive]` or `[one-shot]`). Each prompt remembers the mode it was created with.

## How it works under the hood

1. You type a prompt → it's added to the queue as `Pending` with the current default mode
2. The event loop checks: `active_workers < max_workers`?
3. If yes, the next pending prompt is dispatched to a new worker
4. **Interactive mode:** a PTY is allocated via `portable-pty`, and `claude "prompt" --dangerously-skip-permissions` is spawned inside it. A reader thread feeds PTY output into an `alacritty_terminal` grid. The UI renders the grid with full color and formatting support.
5. **One-shot mode:** the worker spawns `claude -p "prompt" --output-format stream-json` and a reader thread parses streaming deltas as `OutputChunk` messages
6. The UI renders output in real time with auto-scroll
7. When Claude finishes (PTY EOF or process exit), the worker sends `Finished` and the prompt moves to `Completed`
8. For PTY workers, the terminal text is extracted from the grid and stored for export/session persistence
9. The next queued prompt is automatically dispatched

## License

MIT
