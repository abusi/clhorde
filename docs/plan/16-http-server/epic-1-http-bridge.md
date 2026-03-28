# Epic 1: HTTP Bridge (Rust Backend)

## Goal

Create `clhorde-web`, a new Rust crate that acts as a thin HTTP/WebSocket bridge between browser clients and the existing daemon IPC protocol.

## Scope

- New `crates/clhorde-web/` crate using `axum`
- Daemon IPC bridge (reuses `clhorde-core` framing)
- Full REST API covering state, prompts, config, store, and health
- WebSocket endpoint with real-time event fan-out and PTY byte streaming
- Static file serving (embedded + filesystem override)
- CLI args (`--port`, `--host`, `--daemon-socket`, `--static-dir`)

## Dependencies

- `clhorde-core` (IPC types and framing)
- `axum`, `tokio`, `tokio-tungstenite`, `tower-http`, `serde_json`, `clap`, `tracing`, `base64`

## Tickets

| ID | Title | Priority |
|----|-------|----------|
| [M1](epic-1-http-bridge/M1.md) | Crate scaffolding & CLI args | P0 |
| [M2](epic-1-http-bridge/M2.md) | Daemon bridge (IPC client) | P0 |
| [M3](epic-1-http-bridge/M3.md) | REST API — State & health endpoints | P0 |
| [M4](epic-1-http-bridge/M4.md) | REST API — Prompt action endpoints | P0 |
| [M5](epic-1-http-bridge/M5.md) | REST API — Configuration & store endpoints | P1 |
| [M6](epic-1-http-bridge/M6.md) | WebSocket handler & event fan-out | P0 |
| [M7](epic-1-http-bridge/M7.md) | PTY byte streaming over WebSocket | P1 |
| [M8](epic-1-http-bridge/M8.md) | Static file serving | P1 |

## Acceptance Criteria

- `clhorde-web` binary starts, connects to daemon, serves REST + WS
- All `ClientRequest` variants reachable via REST or WebSocket
- WebSocket clients receive `DaemonEvent` stream in real time
- PTY bytes base64-encoded and forwarded to WS clients
- Binds to localhost by default; `--host 0.0.0.0` required for network exposure
