# Phase 6: Add Structured Logging to clhorded Daemon

## Context

The `clhorded` daemon currently uses `eprintln!` for a handful of lifecycle messages (startup, shutdown, signals, IPC errors). There is no way to increase verbosity for debugging. This phase adds a `-v`/`--verbose` flag so operators can see what the daemon is doing (prompt lifecycle, worker spawns, client connections, IPC traffic, etc.).

## Approach

Use `tracing` + `tracing-subscriber` — the standard logging stack for tokio applications. Parse `-v`/`--verbose` manually from args (no clap needed for two flags).

**Verbosity levels:**
- Default (no flag): `warn` — only warnings and errors to stderr
- `-v`: `info` — lifecycle events (startup, prompt added, worker started/finished, client connect/disconnect)
- `-vv`: `debug` — detailed operations (IPC request routing, worktree creation, queue dispatch logic)

## Steps

### 1. Add workspace dependencies

**File:** `Cargo.toml` (workspace root)
```toml
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

**File:** `crates/clhorde-daemon/Cargo.toml`
```toml
tracing.workspace = true
tracing-subscriber.workspace = true
```

### 2. Parse `-v`/`--verbose` in `main.rs` and init subscriber

**File:** `crates/clhorde-daemon/src/main.rs`

- Parse `std::env::args()` for `-v`, `-vv`, `--verbose`, `--help`, `-h`
- Map to verbosity: 0 → `warn`, 1 → `info`, 2+ → `debug`
- Init `tracing_subscriber` with the chosen level filter and stderr writer
- Replace all `eprintln!("clhorded: ...")` with appropriate `tracing::{info,warn,error}!` macros
- Add `--help`/`-h` flag that prints usage and exits

Existing eprintln! locations to convert:
- L56: `"clhorded: {e}"` (PID check fail) → `error!`
- L60: `"clhorded: {e}"` (PID write fail) → `error!`
- L90: `"IPC server error: {e}"` → `error!`
- L101-105: started banner → `info!`
- L143: SIGTERM → `info!`
- L149: SIGINT → `info!`
- L157: killing workers → `info!`
- L178: stopped → `info!`

New log points in main.rs:
- `debug!` for each event loop branch (worker message, client command, session register/unregister)

### 3. Add logging to `orchestrator.rs`

**File:** `crates/clhorde-daemon/src/orchestrator.rs`

Add `use tracing::{info, debug, warn, error};`

Key instrumentation points:

| Location | Level | Message |
|----------|-------|---------|
| `new()` restore | `info!` | `"Restored {n} prompts from disk"` |
| `new()` prune | `debug!` | `"Pruned old prompts (max: {n})"` |
| `add_prompt()` | `info!` | `"Prompt #{id} added: {text_preview}"` (first 60 chars) |
| `dispatch_workers()` pick | `info!` | `"Dispatching prompt #{id} ({mode}), workers: {active}/{max}"` |
| `dispatch_workers()` worktree create | `debug!` | `"Created worktree for #{id}: {path}"` |
| `dispatch_workers()` worktree fail | `warn!` | `"Worktree creation failed for #{id}: {err}"` |
| `dispatch_workers()` spawn error | `error!` | `"Worker spawn failed for #{id}: {err}"` |
| `apply_message()` Finished | `info!` | `"Worker #{id} finished (exit: {code})"` |
| `apply_message()` SpawnError | `error!` | `"Worker #{id} spawn error: {err}"` |
| `apply_message()` SessionId | `debug!` | `"Worker #{id} session: {sid}"` |
| `apply_message()` TurnComplete | `debug!` | `"Worker #{id} turn complete"` |
| `handle_request()` | `debug!` | `"Request from session {sid}: {request_name}"` |
| `kill_worker()` | `info!` | `"Killing worker #{id}"` |
| `delete_prompt()` | `info!` | `"Deleted prompt #{id}"` |
| `resume_prompt()` | `info!` | `"Resuming prompt #{id}"` |
| `store_drop()` | `info!` | `"Store drop '{filter}': removed {n}"` |
| `store_keep()` | `info!` | `"Store keep '{filter}': removed {n}"` |
| `clean_worktrees()` | `info!` | `"Cleaned {n} worktrees"` |
| `maybe_cleanup_worktree()` | `debug!` | `"Auto-cleanup worktree for #{id}: {path}"` |
| `shutdown()` | `info!` | `"Shutting down {n} workers"` |

### 4. Add logging to `ipc_server.rs`

**File:** `crates/clhorde-daemon/src/ipc_server.rs`

Add `use tracing::{info, debug, warn};`

| Location | Level | Message |
|----------|-------|---------|
| `run_server()` listening | `info!` | `"IPC server listening on {path}"` (replace eprintln) |
| `run_server()` accept | `debug!` | `"Client connected (session {id})"` |
| `handle_client()` read error (invalid JSON) | `warn!` | `"Invalid request from session {id}: {err}"` (replace eprintln) |
| `handle_client()` disconnect | `debug!` | `"Client disconnected (session {id})"` |

### 5. Add logging to `worker.rs`

**File:** `crates/clhorde-daemon/src/worker.rs`

Add `use tracing::{debug, error};`

| Location | Level | Message |
|----------|-------|---------|
| `spawn_worker()` PTY mode | `debug!` | `"Spawning PTY worker for #{id}"` |
| `spawn_worker()` OneShot mode | `debug!` | `"Spawning one-shot worker for #{id}"` |
| `spawn_oneshot()` spawn fail | `error!` | `"Failed to spawn claude for #{id}: {err}"` |

### 6. Add logging to `pty_worker.rs`

**File:** `crates/clhorde-daemon/src/pty_worker.rs`

Add `use tracing::{debug, error};`

| Location | Level | Message |
|----------|-------|---------|
| `spawn_pty_worker()` success | `debug!` | `"PTY worker #{id} spawned ({cols}x{rows})"` |
| `spawn_pty_worker()` fail | `error!` | `"PTY worker #{id} failed: {err}"` |
| `resize_pty()` | `debug!` | `"PTY resized to {cols}x{rows}"` |

## Files

**Modified files:**
- `Cargo.toml` — add `tracing`, `tracing-subscriber` to workspace deps
- `crates/clhorde-daemon/Cargo.toml` — add `tracing`, `tracing-subscriber`
- `crates/clhorde-daemon/src/main.rs` — arg parsing, subscriber init, replace eprintln
- `crates/clhorde-daemon/src/orchestrator.rs` — add tracing calls
- `crates/clhorde-daemon/src/ipc_server.rs` — add tracing calls, replace eprintln
- `crates/clhorde-daemon/src/worker.rs` — add tracing calls
- `crates/clhorde-daemon/src/pty_worker.rs` — add tracing calls

## Verification

1. `cargo build --workspace` — zero errors
2. `cargo test --workspace` — all tests pass
3. `cargo clippy --workspace -- -D warnings` — zero warnings
4. `clhorded` — starts quietly (only errors/warnings shown)
5. `clhorded -v` — shows info-level messages (startup, prompt lifecycle, worker events)
6. `clhorded -vv` — shows debug-level messages (IPC requests, dispatch logic, session tracking)
7. `clhorded --help` — prints usage and exits
