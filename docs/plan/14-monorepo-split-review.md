# Monorepo Split Plan Review

Review of `14-monorepo-split.md` — issues, missing components, and open questions.

---

## Critical Architecture Issues

### 1. Multiple TUI clients with different terminal sizes — fundamentally impossible

The plan mentions per-client PTY sizes in `session.rs`, but a single PTY can only have one size at a time. If TUI-A is 80x24 and TUI-B is 120x40, the real PTY master only has one `ioctl(TIOCSWINSZ)`. The daemon-side `Term` and the child process both see one size. The local `Term` instances in each TUI could be different sizes, but the byte stream is generated for the PTY's actual size — you'd get wrapping/truncation artifacts on the mismatched client.

**Resolution:** Drop per-client PTY size tracking from `session.rs`. One global PTY size per prompt, last `ResizePty` wins. Multi-client with different sizes is not a real use case for a local dev tool. Remove `pty_sizes` from `ClientSession`.

### 2. `async fn` in `clhorde-core::ipc` contradicts "no tokio" dependency

The plan says core does NOT depend on tokio, but `ipc.rs` defines `async fn write_message(stream: &mut WriteHalf, ...)` and `async fn read_message(stream: &mut ReadHalf, ...)`. `WriteHalf`/`ReadHalf` are tokio types. Either:
- Core needs a tokio dependency (at least `tokio::io`)
- These functions should live in each consumer crate
- Use a sync API with `std::io::Read/Write` traits instead (both daemon and CLI are tokio-based anyway, so this is awkward)

**Resolution:** Option (c) — core provides pure byte manipulation only: `fn encode_frame(msg: &[u8]) -> Vec<u8>` and `fn decode_frame(buf: &[u8]) -> Result<(usize, Vec<u8>)>`. No async, no I/O, no tokio dependency. Each consumer (daemon, TUI, CLI) wraps these with their own trivial async read/write helpers. Core stays I/O-free.

### 3. Live output streaming for one-shot workers is under-specified

The plan has `PromptInfo` with only `output_len: usize` (no text). For live viewing, the TUI needs the actual text. The plan mentions `OutputChunk` events and a lazy `prompt_outputs: HashMap<usize, String>`, but doesn't specify:
- Are `OutputChunk` events accumulated into `prompt_outputs` in real-time?
- If the user is viewing a prompt in View mode, does the TUI automatically request output?
- What happens if the user switches to View mode mid-stream — do they get a `GetPromptOutput` response with partial text, then start receiving `OutputChunk` deltas?

This is the primary UX path for one-shot workers and needs explicit state machine documentation.

**Resolution:** TUI always accumulates `OutputChunk` events into `prompt_outputs` for all prompts — no eviction, no lazy-loading. Memory cost is negligible (typical Claude output is 2-100KB per prompt; 100 prompts = ~10MB max). `GetPromptOutput` is kept only for the late-join/reconnect path: when a TUI connects to a daemon with already-running prompts, it requests accumulated output for active prompts, then appends subsequent `OutputChunk` events.

### 4. `SendPtyBytes` carries `Vec<u8>` in JSON — serde encoding issue

`ClientRequest::SendPtyBytes { data: Vec<u8> }` will be serialized by serde_json as a JSON array of integers: `[27, 91, 65]` for an up-arrow. This is verbose (3-5 chars per byte) and fragile. Options:
- Use `#[serde(with = "base64")]` or a `ByteBuf` wrapper
- Use the same binary framing in both directions
- Document the encoding explicitly

**Resolution:** Accept as-is. serde_json serializes `Vec<u8>` as an integer array, which is verbose but keystrokes are 1-6 bytes and infrequent. The overhead is invisible. No action needed.

---

## Missing Components from the Plan

### 5. `editor.rs` / `TextBuffer` not mentioned

The plan says `input: String` in the TUI App, but the actual code uses `TextBuffer` from `editor.rs` — a multi-line cursor-aware input buffer. The plan's file inventory doesn't list `editor.rs` at all. This file needs a destination (TUI-only).

**Resolution:** Add `editor.rs` to the plan's file inventory (destination: `clhorde-tui`). Update TUI App struct field from `input: String` to `input: TextBuffer`. Purely a plan omission, no architectural impact.

### 6. Visual select / batch operations missing

The plan omits `visual_select_active: bool`, `selected_ids: HashSet<usize>`, and `confirm_batch_delete`. These are TUI state, but batch delete/retry/move operations need corresponding `ClientRequest` variants (e.g., `DeletePrompts { prompt_ids: Vec<usize> }`).

**Resolution:** TUI loops over `selected_ids` and sends individual `DeletePrompt`/`RetryPrompt` per ID. No batch variants needed in the protocol. These are rare user-initiated actions — 10 small JSON messages over a Unix socket is imperceptible. Add the visual select fields to the plan's TUI App struct.

### 7. `tags` field missing from `PromptInfo`

`Prompt.tags: Vec<String>` is used for `@tag` filter syntax. It's absent from `PromptInfo` in the plan, so the TUI can't filter by tag.

**Resolution:** Add `pub tags: Vec<String>` to `PromptInfo`. Plan omission, no design decision needed.

### 8. `open_external_editor` flag not addressed

The current `main.rs` checks this flag after `handle_key` returns to suspend the terminal and open `$EDITOR`. This coupling between key handler and terminal ownership needs to survive the split — it's TUI-internal so it's fine, but it's absent from the plan's field inventory.

**Resolution:** TUI-internal state, no daemon involvement. Add `open_external_editor: bool` to the plan's TUI App struct. Behavior unchanged from current code.

---

## Potential Runtime Issues

### 9. Prompt ID collision across daemon restarts

`next_id: usize` starts at... what? Currently it's derived from the loaded prompts. If the daemon restarts and loads persisted prompts, `next_id` should be `max(loaded_ids) + 1`. But `id` is an internal runtime concept distinct from `uuid`. If the daemon crashes without persisting, a new daemon could reuse IDs. The `worker_inputs` / `pty_handles` maps key on `id`, so collisions would be catastrophic.

Consider: use `uuid` as the canonical identity everywhere, or persist `next_id` to disk, or derive it from the loaded set on startup.

**Resolution:** Derive `next_id` from loaded set on startup (`max(id) + 1`), accept the theoretical race. Prompts are persisted in `add_prompt()` before workers spawn — the crash window is microseconds, and an unpersisted prompt leaves nothing stale to collide with. `worker_inputs`/`pty_handles` maps are cleared on restart anyway.

### 10. PTY resize race condition

Current: resize is immediate (same process, same `Arc<Mutex>`).
After: TUI detects panel size change → sends `ResizePty` → daemon receives → resizes real PTY. During this window (potentially 1-10ms on Unix socket), PTY output is generated for the old size but the TUI's local `Term` may already be at the new size. This could cause transient rendering glitches (wrapped lines, cursor misplacement).

Mitigation: TUI should resize its local `Term` only *after* receiving confirmation, or accept the glitch as transient.

**Resolution:** Accept the transient mismatch. TUI resizes its local `Term` immediately and sends `ResizePty` to daemon. Round-trip is ~1ms, TUI renders at 100ms ticks, and `alacritty_terminal` handles size mismatches gracefully. This is how every terminal multiplexer works — resizes are inherently asynchronous.

### 11. Late-joining TUI — worse than the plan admits

The plan says "Claude Code redraws frequently during tool use" but this isn't true when Claude is:
- In a long thinking pause (can be 30+ seconds)
- Waiting for user permission input
- Idle after completing work

A user who accidentally closes and reopens the TUI during an interactive session will see an empty PTY grid with no way to recover context until Claude's next screen update. Consider: daemon could buffer the last N bytes (e.g., 64KB — one full screen redraw) and replay to late joiners.

**Resolution:** Daemon maintains a 64KB ring buffer of recent PTY output per active prompt. On late-join (new `Subscribe` + `GetState`), daemon replays the buffered bytes for each `has_pty: true` prompt. The TUI's local `Term` gets an immediate snapshot of the current screen state. Memory cost is negligible (~64KB per active PTY worker, ~320KB total at `max_workers = 5`).

### 12. Synchronous `git` commands still block in daemon

The plan moves worktree creation to the daemon, but the daemon also runs a tokio event loop. `std::process::Command::new("git").output()` blocks the current thread. Should use `tokio::process::Command` or `tokio::task::spawn_blocking`.

**Resolution:** Wrap git commands in `tokio::task::spawn_blocking`. Keeps existing sync code unchanged, just runs it off the async executor. Also fixes the pre-existing issue in the current single-process model.

---

## Design Questions

### 13. Should the daemon own persistence, or should core remain the writer?

Currently `persistence.rs` does synchronous file I/O. In the daemon, this happens in the orchestrator's `add_prompt`/`apply_message` methods. But `clhorde-cli store drop` also writes (deletes files). If both daemon and CLI can write to the same directory concurrently, you need file locking or a convention (CLI only writes when daemon is not running, or CLI goes through the daemon for mutations).

**Resolution:** All CLI commands go through the daemon — both reads and mutations. The CLI is purely a facade, same as the TUI. It does not know where files are stored; storage paths are daemon config. If the daemon is not running, commands fail with a clear error. This means the protocol needs new `ClientRequest` variants for store operations (`StoreList`, `StoreDrop { filter }`, `StoreKeep { filter }`, `StoreCount`, `StorePath`, `CleanWorktrees`) and corresponding `DaemonEvent` response types. The CLI's `commands/store.rs` becomes a thin client, not direct file I/O.

### 14. Daemon auto-start UX during `clhorde-cli` commands

`clhorde-cli store list` doesn't need the daemon — it reads files directly. But `clhorde-cli submit` does. Should `clhorde-cli submit` auto-start the daemon like the TUI does? The plan doesn't specify this.

**Resolution:** No auto-start anywhere. Neither the TUI nor the CLI auto-start the daemon. If `clhorded` is not running, both fail with a clear error: `"Daemon not running. Start it with: clhorded"`. The daemon is an explicit process the user manages. This avoids hidden background process spawning, simplifies the startup logic in both TUI and CLI, and removes the plan's entire "Auto-Start from TUI" section (polling, retry, race conditions with PID files).

### 15. Graceful degradation if daemon dies mid-session

What does the TUI show? A connection error overlay with retry? Does it freeze? Does it attempt to become a standalone process temporarily? The plan mentions reconnect but not the UX during disconnection.

**Resolution:** TUI shows a `[DISCONNECTED]` status bar indicator and retries connection every 2 seconds. User can still quit cleanly (`q`). On successful reconnect, TUI re-subscribes and refreshes full state via `GetState`. No auto-start of daemon — user must restart `clhorded` manually.
