# HTTP Server / Web Interface Plan

**Status: Done**

Breakdown of [web-interface-plan.md](../../web-interface-plan.md) into epics and tickets.

## Epics

| # | Epic | Tickets | Description |
|---|------|---------|-------------|
| 1 | [HTTP Bridge](epic-1-http-bridge.md) | 8 | Rust `clhorde-web` crate — REST API, WebSocket, daemon bridge, static serving |
| 2 | [Dashboard](epic-2-dashboard.md) | 6 | JS frontend — prompt list, submission, config controls, search |
| 3 | [Prompt Detail](epic-3-prompt-detail.md) | 4 | JS frontend — output viewer, xterm.js terminal, input, action controls |
| 4 | [Polish](epic-4-polish.md) | 4 | Store management, error handling, auth, docs & CI integration |

**Total: 22 tickets**

## Dependency Graph

```
Epic 1: HTTP Bridge (Rust)
  M1 Scaffolding ──► M2 Bridge ──► M3 State API ──► M4 Prompt API
                 │            │                      M5 Config/Store API
                 │            └──► M6 WebSocket ──► M7 PTY Streaming
                 └──► M8 Static Serving

Epic 2: Dashboard (JS)          [depends on Epic 1]
  M1 HTML/CSS ──► M2 WS Client ──► M3 Prompt List ──► M6 Filter
                              ├──► M4 Submit Form
                              └──► M5 Config Controls

Epic 3: Prompt Detail (JS)     [depends on Epic 2 + Epic 1 M7]
  M1 ANSI Viewer ──► M3 Follow-up Input
                 └──► M4 Kill/Retry/Resume
  M2 xterm.js

Epic 4: Polish                  [depends on Epics 1–3]
  M1 Store View
  M2 Error Handling
  M3 Auth
  M4 Workspace Integration
```

## Priority Key

- **P0** — Must have, blocks other work
- **P1** — Important, complete within the epic
- **P2** — Nice to have, can defer
