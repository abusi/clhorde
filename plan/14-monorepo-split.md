# Monorepo Split: TUI + Daemon + CLI

**Goal:** Split clhorde from a single binary into 3 processes (TUI, daemon, CLI) and a shared library within a Cargo workspace. This enables persistent worker management (workers survive TUI restarts), headless CLI interaction, and cleaner separation of concerns.

---

## Current Codebase at a Glance

| File | Lines | Destination |
|------|------:|-------------|
| `app.rs` | 2173 | Split: orchestration → daemon, UI state → TUI |
| `cli.rs` | 1223 | → clhorde-cli (commands/) |
| `keymap.rs` | 968 | Split: TOML types + parsing → core, runtime Keymap → TUI |
| `ui.rs` | 994 | → clhorde-tui |
| `pty_worker.rs` | 486 | → clhorde-daemon (modified for byte forwarding) |
| `persistence.rs` | 334 | → clhorde-core |
| `worktree.rs` | 238 | → clhorde-core |
| `prompt.rs` | 224 | → clhorde-core (refactored) |
| `main.rs` | 199 | Split: dispatch loop → daemon, event loop → TUI |
| `worker.rs` | 184 | → clhorde-daemon |
| `editor.rs` | ~200 | → clhorde-tui |
| **Total** | **~7223** | |

---

## Workspace Structure

```
clhorde/
  Cargo.toml                     # workspace root
  crates/
    clhorde-core/                # shared library
      Cargo.toml
      src/
        lib.rs
        prompt.rs                # Prompt, PromptMode, PromptStatus (serializable)
        persistence.rs           # JSON file I/O for prompts
        worktree.rs              # git worktree helpers
        keymap.rs                # TOML config types, parse_key(), key_display(), settings
        config.rs                # path helpers, template loading, history I/O
        protocol.rs              # IPC message types (daemon ↔ TUI/CLI)
        ipc.rs                   # wire framing, socket path resolution
    clhorde-daemon/              # orchestrator daemon
      Cargo.toml
      src/
        main.rs                  # daemon entry, PID file, signal handling, socket server
        orchestrator.rs          # prompt queue, worker dispatch, lifecycle
        worker.rs                # worker spawn (PTY or stream-json)
        pty_worker.rs            # PTY lifecycle, alacritty_terminal grid, byte broadcast
        ipc_server.rs            # Unix socket accept, framing, dispatch
        session.rs               # per-client subscription tracking
    clhorde-tui/                 # terminal UI
      Cargo.toml
      src/
        main.rs                  # terminal setup, daemon connection, event loop
        app.rs                   # UI-only state (modes, selection, input, scroll)
        editor.rs                # TextBuffer: multi-line cursor-aware input buffer
        ui.rs                    # ratatui rendering
        keymap_runtime.rs        # Keymap struct, action enums, build_keymap()
        pty_renderer.rs          # local alacritty_terminal for PTY byte rendering
        ipc_client.rs            # async daemon connection, send/recv
    clhorde-cli/                 # command-line tool
      Cargo.toml
      src/
        main.rs                  # arg parsing, subcommand dispatch
        commands/
          mod.rs
          store.rs               # store list/count/drop/keep/clean-worktrees
          qp.rs                  # quick prompt management
          keys.rs                # keybinding management
          config.rs              # config path/edit/init
          prompt.rs              # NEW: submit prompts to running daemon
          status.rs              # NEW: query daemon status
```

### Binary Names

| Crate | Binary | Purpose |
|-------|--------|---------|
| `clhorde-core` | (library) | Shared types, persistence, config, IPC protocol |
| `clhorde-daemon` | `clhorded` | Background orchestrator, manages workers |
| `clhorde-tui` | `clhorde` | Terminal UI, connects to daemon |
| `clhorde-cli` | `clhorde-cli` | CLI tool for store mgmt + daemon interaction |

---

## Module Placement: What Goes Where

### clhorde-core

**Dependencies:** serde, serde_json, toml, dirs, uuid, chrono, crossterm (KeyCode/KeyModifiers types only)

Modules moved from current codebase:

- **`prompt.rs`** — `Prompt`, `PromptMode`, `PromptStatus`. Refactored: remove `pty_state: Option<SharedPtyState>` (contains `Arc<Mutex<PtyState>>` with alacritty `Term` — not serializable, daemon-only). Replace `started_at`/`finished_at` (`Instant`, not serializable) with `started_at_epoch_ms`/`finished_at_epoch_ms` (`Option<u64>`). The struct becomes fully `Serialize + Deserialize + Clone + Send`.

- **`persistence.rs`** — `PromptFile`, `PromptOptions`, `default_prompts_dir()`, `save_prompt()`, `load_all_prompts()`, `prune_old_prompts()`, `delete_prompt_file()`. Unchanged. Used by daemon only (runtime persistence + store management via IPC). CLI routes all store operations through the daemon.

- **`worktree.rs`** — `is_git_repo()`, `repo_root()`, `repo_name()`, `worktree_exists()`, `create_worktree()`, `remove_worktree()`. Unchanged. Used by daemon only (worktree creation/cleanup). Daemon wraps calls in `tokio::task::spawn_blocking` to avoid blocking the async executor.

- **`keymap.rs`** — The TOML-facing half of the current 968-line `keymap.rs`. Includes:
  - TOML deserialization structs: `TomlConfig`, `TomlSettings`, `TomlKeybindings`, `TomlModeBindings`
  - Parsing/display: `parse_key()`, `key_display()`, `parse_modifier()`
  - Config file I/O: `config_path()`, `load_toml_config()`, `save_toml_config()`, `default_toml_config()`
  - Settings loading: `load_settings()` (returns `TomlSettings`)
  - Action enum string representations (for CLI `keys list`/`keys set`)
  - Does **not** include: `Keymap` struct, `build_keymap()`, action enums with their `From<&str>` impls — those stay in TUI

New modules:

- **`config.rs`** — Extracted from `app.rs::new()`: `data_dir()`, `prompts_dir()`, `history_path()`, `templates_path()`, `load_templates()`, `load_history()`, `save_history()`. Pure path resolution and file I/O — no runtime state.

- **`protocol.rs`** — IPC message types (see [Protocol Types](#protocol-types) below).

- **`ipc.rs`** — Pure byte manipulation for length-delimited framing (no async, no I/O, no tokio dependency). Each consumer (daemon, TUI, CLI) wraps these with their own trivial async read/write helpers:
  - `fn encode_frame(msg: &[u8]) -> Vec<u8>` — prepend 4-byte BE length header
  - `fn decode_frame(buf: &[u8]) -> Result<(usize, Vec<u8>)>` — parse length header + payload
  - `fn daemon_socket_path() -> PathBuf` (`~/.local/share/clhorde/daemon.sock`)
  - `fn daemon_pid_path() -> PathBuf` (`~/.local/share/clhorde/daemon.pid`)
  - `fn encode_pty_frame(prompt_id: usize, data: &[u8]) -> Vec<u8>` — binary PTY frame with 0x01 marker
  - `fn decode_pty_frame(payload: &[u8]) -> Option<(usize, &[u8])>` — extract prompt_id + raw bytes from binary frame
  - `fn is_binary_frame(payload: &[u8]) -> bool` — check first byte for 0x01 marker

### clhorde-daemon

**Dependencies:** clhorde-core, tokio, serde_json, alacritty_terminal, portable-pty, uuid
**Does NOT depend on:** ratatui, crossterm (no terminal UI)

- **`orchestrator.rs`** — Core business logic extracted from `app.rs`. Owns the prompt queue, worker pool, and persistence. Emits `DaemonEvent` to subscribers on every state change. Also absorbs the worker dispatch loop from current `main.rs` lines 82-163 (the `while active_workers < max_workers` loop). See [The Big Split: app.rs](#the-big-split-apprs) for field-by-field breakdown.

- **`worker.rs`** — Current `worker.rs` unchanged: `WorkerMessage`, `WorkerInput`, `SpawnResult` enums, `spawn_worker()` function, one-shot stream-json parsing logic. Worker threads send `WorkerMessage` to orchestrator via `mpsc::UnboundedSender`.

- **`pty_worker.rs`** — Current `pty_worker.rs`, modified. The reader thread gains a second output path: in addition to feeding bytes into the local `alacritty_terminal::Term` (needed for `extract_text_from_term()` on completion), it also sends raw bytes to a `tokio::sync::broadcast` channel that `ipc_server.rs` fans out to subscribers. Additionally, the daemon maintains a **64KB ring buffer** of recent PTY output per active prompt; on late-join, the buffered bytes are replayed to the new client so its local `Term` gets an immediate snapshot of the current screen state. Key types unchanged: `PtyState`, `SharedPtyState`, `PtyHandle`. Functions: `spawn_pty_worker()`, `key_event_to_bytes()`, `extract_text_from_term()`, `resize_pty()`.

- **`ipc_server.rs`** — `tokio::net::UnixListener`, accepts connections, spawns per-client tasks. Each task: read `ClientRequest` frames → dispatch to orchestrator → forward `DaemonEvent` stream back to client. Uses `tokio::sync::broadcast` for fan-out.

- **`session.rs`** — `struct ClientSession { id, subscribed }`. Tracks which clients are subscribed to events. PTY size is global per prompt (last `ResizePty` wins) — no per-client size tracking.

### clhorde-tui

**Dependencies:** clhorde-core, ratatui, crossterm, tokio, alacritty_terminal, serde_json, chrono, dirs
**Does NOT depend on:** portable-pty (daemon owns the PTY)

- **`app.rs`** — UI-only subset of current `app.rs`. Holds `prompts: Vec<PromptInfo>` (lightweight mirror updated via daemon events), all UI state, and `daemon_tx: mpsc::UnboundedSender<ClientRequest>`. All `handle_*_key()` methods modified to send `ClientRequest` instead of directly mutating orchestrator state. See [The Big Split: app.rs](#the-big-split-apprs) for complete field inventory.

- **`ui.rs`** — Current `ui.rs`, adapted. Renders from `Vec<PromptInfo>` instead of `Vec<Prompt>`. PTY grid access changes from `prompt.pty_state.lock()` to `pty_renderer.get_term(prompt_id)`. All sub-renderers (`render_prompt_list`, `render_output_viewer`, `render_pty_grid`, etc.) unchanged in structure.

- **`keymap_runtime.rs`** — The runtime half of current `keymap.rs`. Contains:
  - Action enums: `NormalAction`, `InsertAction`, `ViewAction`, `InteractAction`, `FilterAction`
  - `struct Keymap` with `HashMap<KeyCode, *Action>` per mode + `quick_prompts: HashMap<KeyCode, String>`
  - `build_keymap(toml_config: &TomlConfig) -> Keymap` — constructs from parsed TOML
  - Uses `parse_key()` and action string conversions from `clhorde-core::keymap`

- **`pty_renderer.rs`** — Manages `HashMap<usize, Term<VoidListener>>` for PTY workers. On `DaemonEvent::PtyOutput { prompt_id, data }`, feeds raw bytes to the appropriate local `Term` via `Processor::advance()`. On prompt removal, drops the `Term`. Provides `fn get_term(&self, id: usize) -> Option<&Term<VoidListener>>` for `ui.rs`.

- **`ipc_client.rs`** — Async connection to daemon Unix socket. `async fn connect() -> Client`. Methods: `send(ClientRequest)`, background task reads `DaemonEvent` frames and forwards via `mpsc` channel to the app event loop. Handles reconnect on disconnect (re-subscribes, re-requests full state).

### clhorde-cli

**Dependencies:** clhorde-core, serde_json, tokio (for daemon commands), uuid

All CLI commands go through the daemon — the CLI is purely a facade, same as the TUI. It does not know where files are stored; storage paths are daemon config. If the daemon is not running, all commands fail with a clear error: `"Daemon not running. Start it with: clhorded"`.

Current `cli.rs` subcommands (1223 lines) split into focused modules, all as thin daemon clients:

- **`commands/store.rs`** — `store {list, count, path, drop <filter>, keep <filter>, clean-worktrees}`. Sends corresponding `ClientRequest` variants to daemon.
- **`commands/qp.rs`** — `qp {list, add <key> <msg>, remove <key>}`. Uses `clhorde-core` keymap config (local config file, not daemon).
- **`commands/keys.rs`** — `keys {list [mode], set <mode> <action> <keys...>, reset <mode> [action]}`. Uses `clhorde-core` keymap config (local config file, not daemon).
- **`commands/config.rs`** — `config {path, edit, init [--force]}`. Uses `clhorde-core` config paths (local config file, not daemon).
- **`commands/prompt.rs`** — New: `clhorde-cli submit "prompt text" [--mode interactive|oneshot] [--cwd path]`. Sends `SubmitPrompt` to daemon, prints prompt ID.
- **`commands/status.rs`** — New: `clhorde-cli status`. Sends `GetState` to daemon, prints worker/queue summary table.

---

## IPC: Unix Domain Sockets

### Why Unix Domain Sockets

| Option | Pros | Cons | Verdict |
|--------|------|------|---------|
| **Unix domain sockets** | Low latency (<1ms), bidirectional streaming, file-permission auth, no port conflicts | Unix-only (fine for dev tool) | **Selected** |
| gRPC | Structured, code-gen | Heavy (tonic + prost), overkill for local IPC | Rejected |
| Shared files + signals | Simple | Race conditions, no streaming, no real-time PTY | Rejected |
| Named pipes | Simple | Unidirectional, awkward for bidirectional protocol | Rejected |

### Socket Location

```
~/.local/share/clhorde/daemon.sock    # Unix domain socket
~/.local/share/clhorde/daemon.pid     # PID file for stale detection
```

### Wire Format

Hybrid length-delimited framing:

```
Standard messages: [4 bytes: u32 BE length][JSON payload]
PTY byte stream:   [4 bytes: u32 BE length][0x01 marker][4 bytes: u32 prompt_id][raw PTY bytes]
```

First byte of payload distinguishes: `{` (0x7B) = JSON message, `0x01` = binary PTY frame. This avoids base64 encoding overhead for high-throughput PTY data.

### Protocol Types

```rust
// clhorde-core/src/protocol.rs

use serde::{Serialize, Deserialize};

/// TUI/CLI → Daemon
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum ClientRequest {
    // Prompt management
    SubmitPrompt {
        text: String,
        cwd: Option<String>,
        mode: String,           // "interactive" | "oneshot"
        worktree: bool,
    },
    RetryPrompt { prompt_id: usize },
    ResumePrompt { prompt_id: usize },
    KillWorker { prompt_id: usize },
    MovePromptUp { prompt_id: usize },
    MovePromptDown { prompt_id: usize },
    DeletePrompt { prompt_id: usize },

    // Worker pool
    SetMaxWorkers { count: usize },
    SetDefaultMode { mode: String },

    // Interaction
    SendInput { prompt_id: usize, text: String },           // one-shot follow-up
    SendPtyBytes { prompt_id: usize, data: Vec<u8> },       // PTY keystrokes (JSON, not binary frame)

    // Subscriptions
    Subscribe,                  // start receiving live DaemonEvents
    Unsubscribe,

    // Queries
    GetState,                   // request full DaemonState snapshot
    GetPromptOutput { prompt_id: usize },  // request full output text for one prompt (late-join/reconnect only)

    // Store management (CLI facade)
    StoreList,
    StoreCount,
    StorePath,
    StoreDrop { filter: String },           // "all" | "completed" | "failed" | "pending"
    StoreKeep { filter: String },           // "completed" | "failed" | "pending"
    CleanWorktrees,

    // PTY
    ResizePty { prompt_id: usize, cols: u16, rows: u16 },

    // Lifecycle
    Ping,
    Shutdown,
}

/// Daemon → TUI/CLI
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum DaemonEvent {
    // State sync
    StateSnapshot { state: DaemonState },
    PromptAdded { prompt: PromptInfo },
    PromptUpdated { prompt: PromptInfo },
    PromptRemoved { prompt_id: usize },

    // Output streaming (one-shot workers)
    OutputChunk { prompt_id: usize, text: String },
    PromptOutput { prompt_id: usize, full_text: String },  // response to GetPromptOutput

    // PTY output — sent via binary frame, NOT this enum
    // (listed here for documentation; actual delivery uses 0x01 binary framing)
    // PtyOutput { prompt_id: usize, data: Vec<u8> },

    // Worker lifecycle
    WorkerStarted { prompt_id: usize },
    WorkerFinished { prompt_id: usize, exit_code: Option<i32> },
    WorkerError { prompt_id: usize, error: String },
    TurnComplete { prompt_id: usize },
    SessionId { prompt_id: usize, session_id: String },

    // Pool changes
    MaxWorkersChanged { count: usize },
    ActiveWorkersChanged { count: usize },

    // Store responses
    StoreListResult { prompts: Vec<PromptInfo> },
    StoreCountResult { pending: usize, running: usize, completed: usize, failed: usize },
    StorePathResult { path: String },
    StoreOpComplete { message: String },     // response to StoreDrop/StoreKeep/CleanWorktrees

    // Lifecycle
    Pong,
    Error { message: String },
}

/// Lightweight prompt info for TUI display (no full output text, no runtime types)
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PromptInfo {
    pub id: usize,
    pub text: String,
    pub cwd: Option<String>,
    pub mode: String,              // "interactive" | "oneshot"
    pub status: String,            // "pending" | "running" | "idle" | "completed" | "failed"
    pub output_len: usize,         // byte length of accumulated output (not the text itself)
    pub error: Option<String>,
    pub elapsed_secs: Option<f64>,
    pub seen: bool,
    pub uuid: String,
    pub queue_rank: f64,
    pub session_id: Option<String>,
    pub resume: bool,
    pub worktree: bool,
    pub worktree_path: Option<String>,
    pub has_pty: bool,             // true if this prompt has an active PTY (for rendering decisions)
    pub tags: Vec<String>,         // @tag prefixes for filtering
}

/// Full daemon state for initial sync on Subscribe
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DaemonState {
    pub prompts: Vec<PromptInfo>,
    pub max_workers: usize,
    pub active_workers: usize,
    pub default_mode: String,
}
```

**Note on `SendPtyBytes` vs binary PTY frames:** Keystroke input from TUI→daemon is small (1-6 bytes per keypress) and infrequent, so it uses the JSON `ClientRequest` path. PTY output from daemon→TUI is high-volume (screen redraws), so it uses the binary 0x01 frame path to avoid JSON/base64 overhead.

---

## PTY Forwarding: Raw Bytes Approach

The most architecturally significant decision. Currently the TUI reads PTY state from `Arc<Mutex<PtyState>>` containing an `alacritty_terminal::Term`. With the daemon split:

1. **Daemon** owns the PTY (via `portable-pty`) and reads raw output bytes from the PTY master
2. **Daemon** feeds bytes to its own `alacritty_terminal::Term` (needed for `extract_text_from_term()` on completion — this extracts display text for the output field in persistence)
3. **Daemon** forwards raw bytes to subscribed TUI clients via binary-framed `PtyOutput` messages
4. **TUI** receives raw bytes and feeds them to its own local `alacritty_terminal::Term` via `Processor::advance()`
5. **TUI** renders the grid from its local `Term`, exactly as today (via `render_pty_grid()`)

### Why this works

- TUI already knows how to render from `alacritty_terminal::Term` — minimal UI changes
- Raw PTY bytes are compact (typically 4-8KB chunks for screen redraws)
- Full fidelity: colors, cursor position, scrollback, all ANSI escape sequences preserved
- No complex grid serialization (the grid has ~30 fields per cell across thousands of cells)
- Resize: TUI resizes its local `Term` immediately and sends `ResizePty` to daemon. The transient size mismatch (~1ms round-trip) is invisible — `alacritty_terminal` handles this gracefully, same as real terminal multiplexers
- Multiple TUI clients can connect simultaneously, each with independent local terminal state. PTY size is global per prompt (last `ResizePty` wins)

### PTY reader thread modification

Current (`pty_worker.rs`):
```rust
// Reader thread: PTY master → alacritty Term
loop {
    let n = reader.read(&mut buf)?;
    let mut state = pty_state.lock().unwrap();
    for byte in &buf[..n] {
        state.processor.advance(&mut state.term, *byte);
    }
    tx.send(WorkerMessage::PtyUpdate { prompt_id })?;
}
```

After (daemon):
```rust
// Reader thread: PTY master → alacritty Term + ring buffer + broadcast to subscribers
loop {
    let n = reader.read(&mut buf)?;
    let bytes = buf[..n].to_vec();
    // Feed local Term (for text extraction on finish)
    let mut state = pty_state.lock().unwrap();
    for byte in &bytes {
        state.processor.advance(&mut state.term, *byte);
    }
    // Append to 64KB ring buffer (for late-joining TUI replay)
    ring_buffer.extend(&bytes);
    // Forward raw bytes to subscriber broadcast channel
    let _ = pty_byte_tx.send((prompt_id, bytes));
    tx.send(WorkerMessage::PtyUpdate { prompt_id })?;
}
```

### Batching

Daemon buffers PTY output at 50-100ms intervals or up to 8KB, whichever comes first, to avoid flooding the socket with tiny frames. This matches the TUI's 100ms tick interval — sub-tick latency is invisible.

### Late-joining TUI clients

When a TUI connects to a daemon with already-running PTY workers, it receives:
1. `StateSnapshot` with `has_pty: true` on relevant prompts
2. Replay of the 64KB ring buffer of recent PTY output for each active PTY prompt — the TUI's local `Term` gets an immediate snapshot of the current screen state
3. Subsequent live PTY bytes stream normally

The ring buffer ensures the user sees the current screen even if Claude is in a long thinking pause, waiting for permission, or idle. Memory cost is negligible (~64KB per active PTY worker, ~320KB total at `max_workers = 5`).

---

## Daemon Lifecycle

### No Auto-Start

Neither the TUI nor the CLI auto-start the daemon. The user must start `clhorded` explicitly. This avoids hidden background process spawning, simplifies startup logic, and eliminates polling/retry race conditions.

```
TUI/CLI startup:
  1. Try connect to ~/.local/share/clhorde/daemon.sock
  2. If success → send Ping → wait for Pong → Subscribe → GetState → proceed
  3. If fail → exit with clear error: "Daemon not running. Start it with: clhorded"
```

### Disconnection Handling (TUI)

If the daemon connection is lost mid-session (daemon crash, shutdown, etc.):
1. TUI shows `[DISCONNECTED]` indicator in status bar
2. Retries connection every 2 seconds in the background
3. User can still quit cleanly (`q`)
4. On successful reconnect: re-subscribes, requests full state via `GetState`, requests `GetPromptOutput` for any prompts with `has_pty: false`

### PID File Protocol

```
daemon startup:
  1. Try create daemon.pid with O_CREAT|O_EXCL (atomic)
  2. If exists: read PID, check /proc/<pid>/exe or kill(pid, 0)
     - If alive → another daemon running → exit with error
     - If dead → stale PID file → unlink socket + PID → continue
  3. Write own PID
  4. Bind daemon.sock
  5. On clean shutdown: unlink socket + PID file
```

### Shutdown

- On `Shutdown` request: send `Kill` to all active workers, wait up to 5s for exits, clean up socket + PID file, exit 0
- On SIGTERM/SIGINT: same graceful drain
- On last client disconnect: start configurable idle timer (default 5 min). If timer fires and no workers are active and no clients connected → auto-shutdown. If a new client connects or a worker is still running, cancel the timer.

### Configuration

```toml
# ~/.config/clhorde/keymap.toml
[settings]
daemon_auto_shutdown_minutes = 5    # 0 = never auto-shutdown
daemon_socket_path = ""             # override default socket path
```

---

## The Big Split: app.rs

Current `app.rs` is 2,173 lines mixing orchestration logic with UI state. Here's the precise field-by-field split.

### Fields → Orchestrator (daemon)

```rust
// clhorde-daemon/src/orchestrator.rs

pub struct Orchestrator {
    // Prompt state (the source of truth)
    prompts: Vec<Prompt>,
    next_id: usize,

    // Worker pool
    max_workers: usize,
    active_workers: usize,
    worker_inputs: HashMap<usize, mpsc::UnboundedSender<WorkerInput>>,
    pty_handles: HashMap<usize, PtyHandle>,

    // Configuration
    default_mode: PromptMode,
    max_saved_prompts: usize,
    worktree_cleanup: WorktreeCleanup,
    prompts_dir: Option<PathBuf>,

    // IPC
    subscribers: Vec<mpsc::UnboundedSender<DaemonEvent>>,

    // PTY byte broadcast
    pty_byte_tx: broadcast::Sender<(usize, Vec<u8>)>,

    // Worker message receiver
    worker_rx: mpsc::UnboundedReceiver<WorkerMessage>,
    worker_tx: mpsc::UnboundedSender<WorkerMessage>,  // cloned into each worker
}
```

Methods migrated from `App`:
- `add_prompt(text, cwd, mode, worktree)` — creates prompt, persists, emits `PromptAdded`
- `mark_running(idx)` — sets status to Running, updates timing
- `apply_message(WorkerMessage)` — processes worker output/completion, emits events
- `next_pending_prompt_index()` — finds next dispatchable prompt
- `persist_prompt(idx)` / `persist_prompt_by_id(id)` — JSON file I/O
- `retry_prompt(id)` — reset to Pending with new UUID, emits `PromptAdded`
- `resume_prompt(id)` — set resume flag, reset to Pending, emits `PromptAdded`
- `move_prompt_up(id)` / `move_prompt_down(id)` — reorder queue_rank
- `maybe_cleanup_worktree(id)` — conditional worktree removal
- `resize_pty_workers(cols, rows)` — broadcast resize to all PTY handles
- `pending_count()` / `completed_count()` — status aggregation
- `dispatch_workers()` — the worker dispatch loop (from `main.rs` lines 82-163)

New methods:
- `handle_request(ClientRequest, client_id)` — dispatch incoming IPC requests
- `broadcast(DaemonEvent)` — fan out event to all subscribers
- `to_prompt_info(prompt) -> PromptInfo` — convert Prompt to wire type
- `to_daemon_state() -> DaemonState` — snapshot for GetState
- `run()` — main event loop: `tokio::select!` on worker_rx + ipc commands

### Fields → TUI App

```rust
// clhorde-tui/src/app.rs

pub struct App {
    // --- Daemon state mirror (updated via DaemonEvent) ---
    pub prompts: Vec<PromptInfo>,           // lightweight mirror, no full output
    pub max_workers: usize,
    pub active_workers: usize,
    pub default_mode: String,
    pub prompt_outputs: HashMap<usize, String>,  // accumulated from OutputChunk events (all prompts, no eviction)

    // --- Local PTY rendering ---
    pub pty_terms: HashMap<usize, Term<VoidListener>>,  // managed by pty_renderer
    pub output_panel_size: Option<(u16, u16)>,
    pub last_pty_size: Option<(u16, u16)>,

    // --- Mode & navigation ---
    pub mode: AppMode,
    pub list_state: ListState,              // ratatui selection state
    pub pending_g: bool,                    // waiting for second 'g' in gg sequence
    pub list_height: u16,                   // set during render for page calculations
    pub list_ratio: u16,                    // panel split ratio (10-90)
    pub list_collapsed: bool,               // list panel hidden

    // --- Input buffers ---
    pub input: TextBuffer,                  // Insert mode text (multi-line, cursor-aware)
    pub interact_input: String,             // Interact mode text
    pub filter_input: String,               // Filter mode text

    // --- Output viewer ---
    pub scroll_offset: u16,
    pub auto_scroll: bool,

    // --- Suggestions ---
    pub suggestions: Vec<String>,           // directory path suggestions
    pub suggestion_index: usize,
    pub template_suggestions: Vec<String>,  // template name suggestions
    pub template_suggestion_index: usize,

    // --- Filter ---
    pub filter_text: Option<String>,        // active filter (None = no filter)
    pub filtered_indices: Vec<usize>,       // indices into self.prompts matching filter

    // --- History ---
    pub history: Vec<String>,               // loaded from disk
    pub history_index: Option<usize>,
    pub history_stash: String,              // stashed input when navigating history

    // --- Templates ---
    pub templates: HashMap<String, String>, // loaded from templates.toml

    // --- Visual select / batch ---
    pub visual_select_active: bool,
    pub selected_ids: HashSet<usize>,
    pub confirm_batch_delete: bool,

    // --- Misc UI state ---
    pub tick: u64,                          // 100ms counter for animations
    pub should_quit: bool,
    pub confirm_quit: bool,
    pub status_message: Option<(String, Instant)>,
    pub show_quick_prompts_popup: bool,
    pub worktree_pending: bool,             // Ctrl+W toggle for next prompt
    pub open_external_editor: bool,         // flag for main.rs to suspend terminal and open $EDITOR
    pub daemon_connected: bool,             // false during disconnection, shows [DISCONNECTED] indicator

    // --- Config ---
    pub keymap: Keymap,                     // runtime keybinding dispatch tables

    // --- Daemon connection ---
    pub daemon_tx: mpsc::UnboundedSender<ClientRequest>,
}
```

Methods retained in TUI `App` (modified to send requests):
- All `handle_*_key()` methods: `handle_normal_key()`, `handle_insert_key()`, `handle_view_key()`, `handle_interact_key()`, `handle_pty_interact_key()`, `handle_filter_key()`
- Navigation: `select_next()`, `select_prev()`, `select_first()`, `select_last()`
- Filter: `rebuild_filter()`, `visible_prompt_indices()`
- Suggestions: `update_suggestions()`, `update_template_suggestions()`
- History: `history_prev()`, `history_next()`
- Export: `export_selected_output()` (reads from `prompt_outputs` cache)

New methods:
- `apply_event(DaemonEvent)` — update local state from daemon events. `OutputChunk` events are accumulated into `prompt_outputs` for all prompts (no eviction). `GetPromptOutput` is only used on reconnect/late-join.
- `selected_prompt() -> Option<&PromptInfo>` — get currently selected prompt info

### Key Behavioral Change

Before (current, direct mutation):
```rust
NormalAction::Retry => {
    self.retry_selected();  // directly mutates prompts vec, persists to disk
}
```

After (TUI sends request, daemon responds with event):
```rust
// In TUI handle_normal_key():
NormalAction::Retry => {
    if let Some(p) = self.selected_prompt() {
        let _ = self.daemon_tx.send(ClientRequest::RetryPrompt { prompt_id: p.id });
    }
}

// In TUI apply_event():
DaemonEvent::PromptAdded { prompt } => {
    self.prompts.push(prompt);
    self.rebuild_filter();
}
```

### TUI Event Loop Transformation

Current `main.rs`:
```rust
loop {
    terminal.draw(|f| ui::render(f, &mut app));
    // resize PTY if panel size changed
    // dispatch workers (while pending && slots available)
    tokio::select! {
        ev = event_rx.recv() => app.handle_key(ev),
        msg = worker_rx.recv() => app.apply_message(msg),
        _ = tick.tick() => app.tick += 1,
    }
}
```

After (`clhorde-tui/src/main.rs`):
```rust
loop {
    terminal.draw(|f| ui::render(f, &mut app, &pty_renderer));
    // resize PTY: send ResizePty to daemon if panel size changed
    tokio::select! {
        ev = event_rx.recv() => app.handle_key(ev),
        event = daemon_rx.recv() => {
            match event {
                DaemonEvent::PtyOutput { .. } => unreachable!(), // binary framed, handled below
                other => app.apply_event(other),
            }
        }
        (prompt_id, bytes) = pty_byte_rx.recv() => {
            pty_renderer.feed_bytes(prompt_id, &bytes);
        }
        _ = tick.tick() => {
            app.tick += 1;
            app.clear_expired_status();
        }
    }
}
```

The `ipc_client` background task splits incoming frames: JSON → `daemon_rx`, binary PTY → `pty_byte_rx`.

---

## Cargo Workspace Configuration

```toml
# Cargo.toml (workspace root)
[workspace]
members = ["crates/*"]
resolver = "2"

[workspace.package]
version = "0.2.0"
edition = "2021"
rust-version = "1.88"

[workspace.dependencies]
clhorde-core = { path = "crates/clhorde-core" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
dirs = "6"
chrono = "0.4"
uuid = { version = "1", features = ["v7"] }
tokio = { version = "1", features = ["full"] }
ratatui = "0.30"
crossterm = "0.28"
alacritty_terminal = "0.25"
portable-pty = "0.9"
```

### Per-crate Cargo.toml examples

```toml
# crates/clhorde-core/Cargo.toml
[package]
name = "clhorde-core"
version.workspace = true
edition.workspace = true

[dependencies]
serde.workspace = true
serde_json.workspace = true
toml.workspace = true
dirs.workspace = true
uuid.workspace = true
chrono.workspace = true
crossterm = { workspace = true, default-features = false }  # KeyCode/KeyModifiers only
```

```toml
# crates/clhorde-daemon/Cargo.toml
[package]
name = "clhorde-daemon"
version.workspace = true
edition.workspace = true

[[bin]]
name = "clhorded"
path = "src/main.rs"

[dependencies]
clhorde-core.workspace = true
tokio.workspace = true
serde_json.workspace = true
alacritty_terminal.workspace = true
portable-pty.workspace = true
uuid.workspace = true
```

```toml
# crates/clhorde-tui/Cargo.toml
[package]
name = "clhorde-tui"
version.workspace = true
edition.workspace = true

[[bin]]
name = "clhorde"
path = "src/main.rs"

[dependencies]
clhorde-core.workspace = true
ratatui.workspace = true
crossterm.workspace = true
tokio.workspace = true
alacritty_terminal.workspace = true
serde_json.workspace = true
chrono.workspace = true
dirs.workspace = true
```

```toml
# crates/clhorde-cli/Cargo.toml
[package]
name = "clhorde-cli"
version.workspace = true
edition.workspace = true

[[bin]]
name = "clhorde-cli"
path = "src/main.rs"

[dependencies]
clhorde-core.workspace = true
serde_json.workspace = true
tokio.workspace = true
uuid.workspace = true
```

### Per-crate dependency summary

| Crate | Key deps | Does NOT depend on |
|-------|----------|--------------------|
| **clhorde-core** | serde, serde_json, toml, dirs, uuid, chrono, crossterm (types only) | ratatui, tokio, alacritty_terminal, portable-pty |
| **clhorde-daemon** | core, tokio, alacritty_terminal, portable-pty | ratatui, crossterm |
| **clhorde-tui** | core, ratatui, crossterm, tokio, alacritty_terminal | portable-pty |
| **clhorde-cli** | core, tokio, serde_json | ratatui, crossterm, alacritty_terminal, portable-pty |

---

## Migration Phases

### Phase 0: Workspace Setup (no behavior change) ✅ DONE

1. Create `Cargo.toml` workspace root, move current crate to `crates/clhorde-tui/`
2. Create `clhorde-core` crate: move `prompt.rs`, `persistence.rs`, `worktree.rs`
3. Refactor `Prompt` in core: remove `pty_state`, replace `Instant` fields with epoch millis
4. Move TOML config types + parsing functions from `keymap.rs` into `clhorde-core::keymap`
5. Extract path helpers (`data_dir`, `prompts_dir`, etc.) into `clhorde-core::config`
6. Update all imports across crates. Everything compiles and works identically as before.

**Verification:** `cargo test`, `cargo run` — identical behavior to pre-split.

### Phase 1: Extract CLI Binary ✅ DONE

7. Create `clhorde-cli` crate with `commands/` modules extracted from `cli.rs`
8. Split `cli.rs` (1223 lines) into: `store.rs`, `qp.rs`, `keys.rs`, `config.rs`
9. Remove CLI dispatch from TUI's `main.rs` (currently `cli::run()` intercepts args)
10. Verify config-only subcommands: `clhorde-cli qp list`, `clhorde-cli keys list`, `clhorde-cli config path` (these read local config files, no daemon needed)

**Verification:** Config-only CLI subcommands produce identical output. TUI launches without CLI arg interception. Store commands are deferred to Phase 5 (require running daemon).

### Phase 2: Split App State (refactor only, no IPC) ✅ DONE

11. Split `App` struct into `Orchestrator` + `App` within `clhorde-tui`
12. `App` holds an owned `Orchestrator`, delegates all business logic calls through it
13. Add `protocol.rs` and `ipc.rs` to `clhorde-core` (types only, no networking yet)
14. Add `PromptInfo` conversion: `Orchestrator::to_prompt_info(&Prompt) -> PromptInfo`
15. Refactor `App` key handlers to use `PromptInfo` for reads, delegate mutations to `Orchestrator`

**Verification:** `cargo test`, `cargo run` — still single process, identical behavior. This is the riskiest phase for regressions; thorough manual testing of all modes.

### Phase 3: Build the Daemon ✅ DONE

16. Create `clhorde-daemon` crate: move `Orchestrator`, `worker.rs`, `pty_worker.rs` from TUI
17. Implement `ipc_server.rs`: `UnixListener`, per-client tasks, frame read/write
18. Implement `session.rs`: client tracking, subscription state
19. Modify `pty_worker.rs` reader thread to broadcast raw bytes via `tokio::sync::broadcast`
20. Implement daemon `main.rs`: socket bind, PID file, signal handling, orchestrator event loop
21. Write a standalone test client (simple binary that connects, subscribes, prints events)

**Verification:** `clhorded` starts, accepts connections, test client receives events. Workers spawn and produce output visible through test client.

### Phase 4: TUI Connects to Daemon

22. Implement `ipc_client.rs` in TUI: async connect, frame splitting (JSON vs binary PTY)
23. Implement `pty_renderer.rs`: local `Term` instances, `feed_bytes()`, `get_term()`
24. Modify TUI `App` to send `ClientRequest` via `daemon_tx` instead of calling `Orchestrator`
25. Add `apply_event(DaemonEvent)` method to update local `PromptInfo` mirror
26. Modify TUI `main.rs`: connect to daemon (fail with clear error if not running), subscribe, new event loop with `tokio::select!`, disconnection handling with `[DISCONNECTED]` indicator and 2s retry
27. Remove `Orchestrator`, `worker.rs`, `pty_worker.rs` from TUI crate (now daemon-only)
28. End-to-end testing: start `clhorded` → start TUI → submit prompt → see output

**Verification:** Full workflow: submit, view, interact, retry, resume, kill, filter, export. PTY rendering works. Multiple TUI connections to same daemon.

### Phase 5: New CLI Commands + Polish

29. Add `clhorde-cli store {list, count, path, drop, keep, clean-worktrees}` — all go through daemon via new `ClientRequest` variants
30. Add `clhorde-cli submit "prompt"` — connect to daemon, submit, print ID
31. Add `clhorde-cli status` — connect, GetState, print table
32. Add `clhorde-cli attach <id>` — connect, subscribe, stream output to stdout
33. Add daemon connection check to all CLI commands: helpful error if daemon not running (`"Daemon not running. Start it with: clhorded"`)
34. Documentation update: README, CLAUDE.md, help text

**Verification:** CLI commands work against running daemon. Store commands produce identical output to pre-split. `clhorde-cli submit` + `clhorde-cli status` shows prompt. `clhorde-cli attach` streams live output.

---

## Risks and Mitigations

| Risk | Impact | Mitigation |
|------|--------|-----------|
| **PTY latency over socket** | Sluggish interactive sessions | Unix sockets are <1ms RTT. Current tick is 100ms. Batching at 50-100ms. Invisible overhead. Benchmark before/after. |
| **State divergence (TUI vs daemon)** | UI shows stale data, confusing UX | TUI treats daemon events as single source of truth. Never caches or assumes. `GetState` available for full resync on reconnect. |
| **Daemon crash → orphan workers** | `claude` processes leak | PTY master FD drop sends SIGHUP to child. Daemon uses `setsid` + process groups. PID file enables detection of stale daemon. User restarts `clhorded` which starts fresh (clean slate). TUI auto-reconnects on 2s retry. |
| **Daemon crash → lost in-flight output** | Partial prompt results lost | Daemon persists prompt state to JSON on every status transition. On restart, completed/failed prompts are restored from disk. Running prompts are lost (acceptable: user can retry). |
| **Backward compatibility** | Users must learn to start `clhorded` first | Clear error message on TUI/CLI startup if daemon not running. README documents the new workflow. Daemon is a simple background process (`clhorded &` or systemd user service). |
| **Large refactor risk** | Regressions in modes, keybindings, edge cases | 5-phase approach. Each phase is independently shippable and testable. Phase 2 (internal split) catches most bugs before IPC is involved. Comprehensive manual testing checklist per phase. |
| **Socket permission issues** | TUI can't connect in some environments | Socket in user-owned `~/.local/share/`. No root required. Configurable socket path for edge cases. Clear error messages with path shown. |
| **Multiple daemon instances** | Conflicting state, port stomping | PID file with atomic create prevents duplicates. Socket path is deterministic per user. |

---

## What Doesn't Change

Some things remain identical through the split:

- **Keybindings and modes** — All vim-style bindings (Normal, Insert, View, Interact, PtyInteract, Filter) work exactly as today. The keymap.toml format is unchanged.
- **Persistence format** — UUID v7 JSON files in `~/.local/share/clhorde/prompts/` are unchanged. Prompts saved by old version load in new version and vice versa.
- **Claude CLI invocation** — Same `claude` and `claude -p` commands with same flags. Worker spawn logic is unchanged.
- **PTY rendering** — Same `alacritty_terminal` grid → `ratatui` Span conversion. Same color/flag mapping. Users see identical output.
- **Git worktree behavior** — Same `git worktree add --detach` pattern. Same cleanup policies.
- **Templates and history** — Same files, same format, same loading logic.
- **`clhorde qp` / `clhorde keys` / `clhorde config`** — Same subcommands, same output. Just invoked as `clhorde-cli` instead (or aliased). These operate on local config files and don't require the daemon.
- **`clhorde store`** — Same subcommands, same output. Now invoked as `clhorde-cli store` and routed through the daemon (requires `clhorded` running).
