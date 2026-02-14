# clhorde

A lightweight, single-binary TUI for orchestrating multiple Claude Code CLI instances in parallel. Built with Rust, ratatui, and crossterm.

![Rust](https://img.shields.io/badge/rust-2021-orange)

## Why clhorde?

Tools like [Claude Squad](https://github.com/smtg-ai/claude-squad), [Clark](https://github.com/brianirish/clark), and [VibeMux](https://github.com/UgOrange/vibemux) solve a similar problem—running multiple Claude Code sessions at once. They're great tools, but they all share the same architecture: **tmux sessions + git worktrees**. Each "instance" is a full interactive Claude Code session wrapped in a tmux pane.

clhorde takes a fundamentally different approach:

| | clhorde | tmux-based tools |
|---|---|---|
| **Architecture** | Direct subprocess control via `--stream-json` | Wraps interactive sessions in tmux panes |
| **Work model** | Prompt queue + worker pool | N independent sessions |
| **Concurrency** | Queue any number of prompts, workers pull from queue | Fixed number of parallel sessions |
| **Dependencies** | Single binary (just needs `claude` in PATH) | Requires tmux, git worktrees |
| **Code isolation** | Each worker is a fresh subprocess—no shared state | Git worktrees per session |
| **Interaction** | Send follow-up messages to running workers | Full interactive terminal per session |
| **Binary size** | ~900 lines of Rust | Go/Python projects with larger dependency trees |
| **Runtime** | Native async (tokio) + OS threads | tmux + shell processes |

### The queue model

Most multi-Claude tools give you N parallel sessions and you manually assign work to each one. clhorde works differently: you queue up as many prompts as you want, and a configurable pool of workers (1–20) pulls from the queue automatically. This means you can batch 50 prompts and walk away—workers will chew through them at whatever concurrency you set.

### Direct protocol integration

Instead of wrapping an interactive terminal, clhorde communicates with Claude directly through the `stream-json` protocol. It spawns `claude` with `--input-format stream-json --output-format stream-json`, parses streaming deltas in real time, and pipes follow-up messages as structured JSON. No terminal emulation layer, no tmux, no screen scraping.

### Truly zero dependencies (beyond `claude`)

No tmux. No git worktrees. No Python. No Node. Just a single Rust binary and the `claude` CLI. Install and run.

## Features

- **Worker pool with queue** — queue unlimited prompts, configure 1–20 concurrent workers with `+`/`-`
- **Real-time streaming** — output streams token-by-token as Claude generates it
- **Interactive follow-ups** — send messages to running workers mid-conversation
- **Vim-style modal interface** — Normal, Insert, View, and Interact modes
- **Live status dashboard** — active workers, queue depth, completed count, per-prompt elapsed time
- **Auto-scroll** — follows output in real time, toggleable with `f`
- **Kill workers** — terminate a running prompt with `x`
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
clhorde
```

That's it. You'll see the TUI. Press `i` to start typing a prompt.

## Keybindings

### Normal mode
| Key | Action |
|-----|--------|
| `i` | Enter insert mode (type a prompt) |
| `j` / `k` / `↑` / `↓` | Navigate prompt list |
| `Enter` | View selected prompt's output |
| `+` / `-` | Increase / decrease max workers (1–20) |
| `q` | Quit |

### Insert mode
| Key | Action |
|-----|--------|
| `Enter` | Submit prompt to queue |
| `Esc` | Cancel and return to normal mode |

### View mode
| Key | Action |
|-----|--------|
| `j` / `k` / `↑` / `↓` | Scroll output |
| `s` | Enter interact mode (send follow-up) |
| `f` | Toggle auto-scroll |
| `x` | Kill running worker |
| `Esc` / `q` | Back to normal mode |

### Interact mode
| Key | Action |
|-----|--------|
| `Enter` | Send follow-up message to worker |
| `Esc` | Back to view mode |

## Architecture

```
┌─────────────────────────────────────────────────────┐
│                    main.rs                          │
│  Terminal setup, tokio::select! event loop          │
│  Dispatches queued prompts → worker pool            │
├─────────────┬───────────────────┬───────────────────┤
│   app.rs    │     ui.rs         │    worker.rs      │
│  State +    │  Stateless        │  Subprocess mgmt  │
│  keybinds   │  ratatui render   │  stream-json I/O  │
├─────────────┴───────────────────┴───────────────────┤
│                  prompt.rs                          │
│  Data model: id, text, status, output, timing       │
└─────────────────────────────────────────────────────┘
```

- **Event handling** — Crossterm events are read on a dedicated OS thread (not async) and forwarded via channel, so the tokio runtime never blocks.
- **Worker threads** — Each `claude` subprocess runs in `std::thread` with separate reader and writer threads for stdout parsing and stdin writing.
- **Communication** — Workers send `WorkerMessage` variants (OutputChunk, Finished, SpawnError) back to the app via `tokio::sync::mpsc`. The app sends `WorkerInput` (SendInput, Kill) to workers.
- **Clean shutdown** — Dropping the stdin writer signals EOF to the `claude` process, which exits gracefully. No SIGKILL needed.

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

## How it works under the hood

1. You type a prompt → it's added to the queue as `Pending`
2. The event loop checks: `active_workers < max_workers`?
3. If yes, the next pending prompt is dispatched to a new worker
4. The worker spawns `claude -p --stream-json` with the prompt as a JSON user message
5. A reader thread parses streaming deltas and sends them back as `OutputChunk` messages
6. The UI renders chunks in real time with auto-scroll
7. When Claude finishes, the worker sends `Finished` and the prompt moves to `Completed`
8. The next queued prompt is automatically dispatched

## License

MIT
