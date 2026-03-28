# Epic 2: JS Frontend — Dashboard

## Goal

Build the main dashboard view — a single-page application that shows the prompt list, allows submission, and updates in real time via WebSocket.

## Scope

- Vanilla JS SPA with no build step
- Dark theme matching TUI aesthetic
- Real-time prompt list with status indicators
- Prompt submission form (text, mode, worktree, cwd)
- Worker count controls
- Filter/search prompts

## Dependencies

- Epic 1 (HTTP bridge must serve REST + WS + static files)

## Tickets

| ID | Title | Priority |
|----|-------|----------|
| [M1](epic-2-dashboard/M1.md) | HTML/CSS scaffold & dark theme | P0 |
| [M2](epic-2-dashboard/M2.md) | WebSocket client & state management | P0 |
| [M3](epic-2-dashboard/M3.md) | Prompt list view | P0 |
| [M4](epic-2-dashboard/M4.md) | Prompt submission form | P0 |
| [M5](epic-2-dashboard/M5.md) | Worker count & config controls | P1 |
| [M6](epic-2-dashboard/M6.md) | Filter & search | P1 |

## Acceptance Criteria

- Dashboard loads at `http://localhost:3120/`
- Prompt list updates in real time as prompts are submitted, started, and completed
- Can submit prompts from the browser and see them appear in the list
- Worker count adjustable from the UI
- Filter/search narrows the prompt list
