# Epic 4: Polish & Integration

## Goal

Complete the web interface with store management, robust error handling, optional authentication, and full project integration.

## Scope

- Store management view
- Error handling and reconnection UX
- Optional token-based authentication for non-localhost use
- Workspace, CI, and documentation updates

## Dependencies

- Epics 1–3

## Tickets

| ID | Title | Priority |
|----|-------|----------|
| [M1](epic-4-polish/M1.md) | Store management view | P1 |
| [M2](epic-4-polish/M2.md) | Error handling & reconnection UX | P1 |
| [M3](epic-4-polish/M3.md) | Token-based authentication | P2 |
| [M4](epic-4-polish/M4.md) | Workspace integration & documentation | P1 |

## Acceptance Criteria

- Store management accessible from the web UI
- Errors (daemon down, WS disconnect) show clear UI feedback with auto-recovery
- Non-localhost deployments can require an auth token
- `clhorde-web` included in CI builds, release binaries, and docs
