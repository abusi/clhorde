# Web Interface & HTTP Bridge Plan

## Overview

Add a web-based view for clhorde and an HTTP server (`clhorde-web`) that translates HTTP/WebSocket calls into daemon IPC commands. This gives users a browser-based alternative to the TUI, enables remote access, and opens the door for richer UI features (syntax highlighting, markdown rendering, responsive layout).

## Architecture

```
Browser (JS SPA)
    │
    ├── REST  ──► clhorde-web (HTTP server) ──► daemon.sock (Unix IPC)
    └── WS    ──► clhorde-web (WebSocket)   ──► daemon.sock (Unix IPC)
```

`clhorde-web` is a new crate in the workspace. It acts as a thin bridge:

- Connects to `clhorded` via the existing Unix domain socket IPC protocol
- Exposes a REST API for request/response commands (submit, status, kill, retry, etc.)
- Exposes a WebSocket endpoint for real-time streaming (output chunks, PTY bytes, state updates)
- Serves the static JS frontend assets

The JS frontend is a single-page application served by the same binary. No separate build step required at runtime — assets are either embedded at compile time or served from a static directory.

## New Crate: `clhorde-web`

```
crates/
└── clhorde-web/
    ├── Cargo.toml
    ├── src/
    │   ├── main.rs           # HTTP server entry, CLI args (--port, --host, --static-dir)
    │   ├── routes.rs         # REST endpoint handlers
    │   ├── ws.rs             # WebSocket connection handler, event fan-out
    │   ├── bridge.rs         # Daemon IPC client, translates HTTP→ClientRequest, DaemonEvent→JSON
    │   └── state.rs          # Shared server state (daemon connection, active WS sessions)
    └── static/               # JS frontend (SPA)
        ├── index.html
        ├── app.js            # Main application logic
        ├── style.css         # Styles
        └── lib/              # Vendored dependencies (if any)
```

### Binary: `clhorde-web`

| Flag | Default | Description |
|------|---------|-------------|
| `--port` | `3120` | HTTP listen port |
| `--host` | `127.0.0.1` | Bind address (localhost only by default for security) |
| `--static-dir` | (embedded) | Override path to serve static files from |
| `--daemon-socket` | auto-detected | Path to daemon socket |

## REST API

All endpoints return JSON. Errors use standard HTTP status codes with `{ "error": "message" }` body.

### State & Status

| Method | Path | Maps to | Description |
|--------|------|---------|-------------|
| `GET` | `/api/state` | `GetState` | Full daemon state snapshot |
| `GET` | `/api/prompts` | `GetState` | List all prompts (extracted from state) |
| `GET` | `/api/prompts/:id` | `GetState` | Single prompt info |
| `GET` | `/api/prompts/:id/output` | `GetPromptOutput` | Full output text for a prompt |

### Prompt Actions

| Method | Path | Maps to | Description |
|--------|------|---------|-------------|
| `POST` | `/api/prompts` | `SubmitPrompt` | Submit a new prompt |
| `POST` | `/api/prompts/:id/input` | `SendInput` | Send follow-up input to running prompt |
| `POST` | `/api/prompts/:id/kill` | `KillWorker` | Kill running worker |
| `POST` | `/api/prompts/:id/retry` | `RetryPrompt` | Retry failed/completed prompt |
| `POST` | `/api/prompts/:id/resume` | `ResumePrompt` | Resume with `--resume` |
| `DELETE` | `/api/prompts/:id` | `DeletePrompt` | Delete a prompt |
| `POST` | `/api/prompts/:id/move-up` | `MovePromptUp` | Move pending prompt up in queue |
| `POST` | `/api/prompts/:id/move-down` | `MovePromptDown` | Move pending prompt down in queue |

### Configuration

| Method | Path | Maps to | Description |
|--------|------|---------|-------------|
| `PUT` | `/api/config/max-workers` | `SetMaxWorkers` | Set max worker count |
| `PUT` | `/api/config/default-mode` | `SetDefaultMode` | Set default prompt mode |
| `PUT` | `/api/prompts/:id/mode` | `SetPromptMode` | Set mode for a specific prompt |

### Store

| Method | Path | Maps to | Description |
|--------|------|---------|-------------|
| `GET` | `/api/store` | `StoreList` | List persisted prompts |
| `GET` | `/api/store/count` | `StoreCount` | Counts by state |
| `GET` | `/api/store/path` | `StorePath` | Storage directory path |
| `POST` | `/api/store/drop` | `StoreDrop` | Drop prompts by filter |
| `POST` | `/api/store/keep` | `StoreKeep` | Keep prompts by filter |
| `POST` | `/api/store/clean-worktrees` | `CleanWorktrees` | Clean lingering worktrees |

### Health

| Method | Path | Maps to | Description |
|--------|------|---------|-------------|
| `GET` | `/api/health` | `Ping` | Health check (returns `Pong` status) |

## WebSocket API

### Endpoint: `GET /api/ws`

Upgrades to WebSocket. Used for:

1. **Real-time events**: After connecting, the server subscribes to the daemon and forwards all `DaemonEvent` messages as JSON to the WebSocket client.

2. **Client commands**: The client can send `ClientRequest`-shaped JSON messages through the WebSocket as an alternative to REST calls.

3. **Output streaming**: `OutputChunk` and `PtyUpdate` events are forwarded in real time for live prompt monitoring.

### Message format (server → client)

```json
{ "type": "DaemonEvent", "event": { "type": "OutputChunk", "prompt_id": 1, "text": "..." } }
```

### Message format (client → server)

```json
{ "type": "ClientRequest", "request": { "type": "SubmitPrompt", "text": "...", "mode": "one-shot", "worktree": false, "tags": [] } }
```

### PTY byte streaming

For PTY-based prompts, raw bytes are base64-encoded and sent as:

```json
{ "type": "PtyBytes", "prompt_id": 1, "data": "<base64>" }
```

The JS frontend can feed these into xterm.js for full terminal rendering.

## JS Frontend

### Technology choices

- **Vanilla JS + minimal dependencies** — keep it simple, no build step required
- **xterm.js** (vendored or CDN) — terminal rendering for PTY-based interactive prompts
- **CSS custom properties** — dark theme matching the TUI aesthetic

### Views

1. **Dashboard** — prompt list with status indicators, worker count, queue controls
   - Submit new prompts (text area + mode selector + worktree toggle)
   - Filter/search prompts
   - Adjust max workers
   - Real-time status updates via WebSocket

2. **Prompt Detail** — output viewer for a selected prompt
   - One-shot prompts: rendered text output with ANSI color support
   - Interactive prompts: xterm.js terminal emulator fed by PTY byte stream
   - Send follow-up input
   - Kill / retry / resume controls

3. **Store** — view and manage persisted prompts
   - List with counts by status
   - Drop/keep actions

### Responsiveness

The web UI should work on desktop and tablet. Mobile is not a priority but shouldn't be broken.

## Implementation Plan

### Phase 1: HTTP bridge (Rust)

1. Create `crates/clhorde-web/` crate with `axum` as the HTTP framework
2. Implement `bridge.rs` — reuse the IPC framing from `clhorde-core` to connect to daemon
3. Implement REST routes that translate to `ClientRequest` and await the corresponding `DaemonEvent` response
4. Implement WebSocket handler with daemon event fan-out
5. Add static file serving (embed with `include_dir` or serve from filesystem)
6. Add `--port`, `--host`, `--daemon-socket` CLI args

### Phase 2: JS frontend — Dashboard

1. Build the prompt list view with real-time WebSocket updates
2. Prompt submission form (text, mode, worktree, cwd)
3. Worker count controls
4. Filter/search
5. Status indicators and auto-refresh

### Phase 3: JS frontend — Prompt detail & terminal

1. Output viewer with ANSI rendering for one-shot prompts
2. xterm.js integration for interactive PTY prompts
3. Input sending for follow-up messages
4. Kill/retry/resume controls

### Phase 4: Polish & integration

1. Store management view
2. Error handling and reconnection logic
3. Authentication option (optional token-based auth for non-localhost use)
4. Add `clhorde-web` to workspace `Cargo.toml`
5. Update CLAUDE.md and README.md

## Dependencies (Rust)

| Crate | Purpose |
|-------|---------|
| `clhorde-core` | Shared IPC types and framing |
| `axum` | HTTP framework (async, tower-based) |
| `tokio` | Async runtime (already in workspace) |
| `tokio-tungstenite` | WebSocket support for axum |
| `tower-http` | Static file serving, CORS |
| `serde_json` | JSON serialization (already in workspace) |
| `clap` | CLI argument parsing |
| `tracing` | Logging (already in workspace) |
| `base64` | PTY byte encoding for WebSocket |

## Security Considerations

- **Bind to localhost by default** — no network exposure without explicit `--host 0.0.0.0`
- **Optional auth token** — when binding to non-localhost, require `--auth-token` or environment variable, checked via `Authorization` header
- **No daemon socket exposure** — the HTTP server never exposes the raw socket; all access is mediated through typed API endpoints
- **CORS** — disabled by default, configurable for development
- **Input validation** — validate all client inputs before translating to `ClientRequest`

## Open Questions

1. **Asset embedding vs. filesystem serving?** Embedding via `include_dir` makes a single binary but increases compile times. Filesystem serving is simpler for development. Could support both with `--static-dir` override.

2. **xterm.js delivery** — vendor into the repo or load from CDN? Vendoring keeps it self-contained but adds ~1MB to the repo.

3. **Authentication scope** — is token auth sufficient or should we support something more robust for team use cases?

4. **Should `clhorde-web` be startable from `clhorded` directly** (e.g., `clhorded --web --port 3120`) or always a separate process? Separate process is simpler and follows the existing pattern.
