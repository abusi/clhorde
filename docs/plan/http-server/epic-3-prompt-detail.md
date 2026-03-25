# Epic 3: JS Frontend — Prompt Detail & Terminal

## Goal

Build the prompt detail view — output rendering for one-shot prompts (ANSI text), xterm.js terminal emulation for interactive PTY prompts, and controls for interacting with running workers.

## Scope

- Output viewer with ANSI color rendering for one-shot prompts
- xterm.js integration for interactive PTY prompts
- Follow-up input for running prompts
- Kill / retry / resume action buttons

## Dependencies

- Epic 2 (dashboard provides navigation to prompt detail)
- Epic 1 M7 (PTY byte streaming over WebSocket)

## Tickets

| ID | Title | Priority |
|----|-------|----------|
| [M1](epic-3-prompt-detail/M1.md) | Output viewer with ANSI rendering | P0 |
| [M2](epic-3-prompt-detail/M2.md) | xterm.js integration for PTY prompts | P0 |
| [M3](epic-3-prompt-detail/M3.md) | Follow-up input | P1 |
| [M4](epic-3-prompt-detail/M4.md) | Kill / retry / resume controls | P1 |

## Acceptance Criteria

- One-shot prompt output renders with ANSI colors
- Interactive prompt output renders in a full terminal emulator (xterm.js)
- Can send follow-up input to running prompts
- Can kill, retry, or resume prompts from the detail view
