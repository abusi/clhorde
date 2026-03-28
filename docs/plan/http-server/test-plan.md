# Test Plan: clhorde-web Frontend

## Overview

This plan covers the most probable user flows through the clhorde web dashboard, organized by priority. Each flow maps to the epic/milestone structure and includes both manual verification steps and automated JS unit test coverage.

## User Flow 1: First Visit & Connection (Epic 2 M1, M2)

**Scenario:** User opens `http://localhost:3120/` for the first time.

1. Page loads with dark theme, sidebar + content layout
2. WebSocket connects to `/api/ws` automatically
3. Connection status dot turns green, text shows "connected"
4. If daemon is down: status dot turns red, reconnect banner appears with "Reconnecting to daemon..."
5. On reconnect: banner disappears, full state re-hydrated from `StateSnapshot`

**Unit tests:**
- `DaemonClient` dispatches `ConnectionStatus` events on connect/disconnect
- `DaemonClient` schedules reconnect with exponential backoff (1s, 2s, 4s, max 30s)
- `DaemonClient.send()` returns false when not connected
- `AppState.connected` tracks connection status from `ConnectionStatus` events

---

## User Flow 2: Submit a Prompt (Epic 2 M4)

**Scenario:** User types a prompt and submits it.

1. User types prompt text in the textarea
2. Selects mode (interactive / one-shot) and optionally toggles worktree
3. Clicks Submit or presses `Ctrl+Enter`
4. `POST /api/prompts` fires with `{ text, mode, worktree, cwd }`
5. Form clears on success, error shown inline on failure
6. New prompt appears in sidebar via `PromptAdded` WS event

**Unit tests:**
- Submit button is disabled during in-flight request
- Form clears on successful submission
- Error message shown on failed submission
- `Ctrl+Enter` triggers submit

---

## User Flow 3: View Prompt List & Filter (Epic 2 M3, M6)

**Scenario:** User browses and filters the prompt list.

1. Sidebar shows all prompts sorted: running > pending > idle > completed > failed
2. Each row shows ID, status badge, mode indicator, truncated text, `[WT]` if worktree
3. User types in search box — list filters by case-insensitive substring match
4. User clicks status chip (e.g., "Running") — list filters by status
5. Both filters combine (text + status)
6. Match count updates (e.g., "3 of 12 prompts")
7. Pressing Escape clears the search

**Unit tests:**
- `AppState._sortPrompts()` sorts by status priority then by ID descending
- Prompt filtering by text (case-insensitive substring)
- Prompt filtering by status
- Combined text + status filtering
- Empty state text when no prompts vs. no matches

---

## User Flow 4: View Prompt Output (Epic 3 M1)

**Scenario:** User clicks a one-shot prompt to view its output.

1. Click prompt row in sidebar — row highlights, URL hash updates to `#prompt-{id}`
2. Detail view shows: header (ID, status badge, mode, worktree, elapsed time)
3. Full output fetched via `GET /api/prompts/:id/output`
4. ANSI escape codes rendered as colored HTML spans
5. Live `OutputChunk` events append to output in real time
6. Auto-scroll follows new output (toggleable)

**Unit tests:**
- `ansiToHtml()` handles reset, bold, dim, italic, underline, strikethrough
- `ansiToHtml()` handles 4-bit foreground/background colors
- `ansiToHtml()` handles 256-color and true-color sequences
- `ansiToHtml()` escapes HTML entities in text
- `ansiToHtml()` skips non-SGR escape sequences (cursor movement etc.)
- `AppState._applyEvent()` appends `OutputChunk` text to prompt

---

## User Flow 5: Prompt Actions — Kill, Retry, Resume, Delete (Epic 3 M4)

**Scenario:** User takes actions on prompts from the detail view.

1. Running prompt shows Kill button — confirm dialog before kill
2. Completed/failed prompt shows Retry and Resume buttons
3. Pending prompt shows Move Up / Move Down buttons
4. All prompts show Delete button — confirm dialog before delete
5. Buttons disabled while action is in-flight
6. After kill/delete, WS events update the prompt list

**Unit tests:**
- `renderActions()` shows correct buttons per status (running, completed, failed, pending)
- Action routes map to correct HTTP methods and URLs

---

## User Flow 6: Interactive Prompt with xterm.js (Epic 3 M2)

**Scenario:** User selects an interactive prompt and interacts with the PTY.

1. Selecting an interactive prompt shows xterm.js terminal (not ANSI viewer)
2. `SubscribePty` sent via WS for the selected prompt
3. Base64 PTY bytes decoded and written to terminal
4. Keyboard input forwarded to daemon via `SendBytes`
5. Terminal resizes with container (FitAddon + `ResizePty` message)
6. Switching to another prompt: `UnsubscribePty` sent, terminal disposed

**Unit tests:**
- PTY base64 decode logic produces correct Uint8Array
- `detachTerminal()` cleans up subscriptions and observers

---

## User Flow 7: Worker Config Controls (Epic 2 M5)

**Scenario:** User adjusts worker count and default mode.

1. Footer shows current max workers and active workers
2. Click `+` — `PUT /api/config/max-workers` with count + 1 (max 20)
3. Click `-` — `PUT /api/config/max-workers` with count - 1 (min 1)
4. Click mode toggle — `PUT /api/config/default-mode` flips interactive/one-shot
5. Changes confirmed via `MaxWorkersChanged` / state update WS events

**Unit tests:**
- Worker increment capped at 20, decrement capped at 1
- Mode toggle flips between "interactive" and "one-shot"

---

## User Flow 8: Follow-up Input (Epic 3 M3)

**Scenario:** User sends follow-up text to a running one-shot prompt.

1. Running one-shot prompt shows input bar at bottom
2. User types message and presses Enter (or clicks Send)
3. `POST /api/prompts/:id/input` with `{ "text": "..." }`
4. Input clears on success, flash animation confirms
5. Input bar hidden for completed/pending/interactive prompts

**Unit tests:**
- Input bar visibility logic per prompt status and mode

---

## User Flow 9: Store Management (Epic 4 M1)

**Scenario:** User navigates to the Store tab to manage persisted prompts.

1. Click "Store" tab — sidebar/content hidden, store view shown
2. Fetches data from `GET /api/store`, `/api/store/count`, `/api/store/path`
3. Status count cards displayed (Pending, Running, Completed, Failed)
4. Prompt list with ID, status, mode, text
5. Action buttons: Drop All, Drop Completed, Drop Failed, Keep Completed, Clean Worktrees
6. Destructive actions (drop) require confirm dialog
7. Store data refreshes after any action

**Unit tests:**
- Store action routes map to correct URLs and bodies
- Confirm required for drop actions, not for refresh/clean

---

## User Flow 10: Authentication (Epic 4 M3)

**Scenario:** Server requires auth token for non-localhost access.

1. On load, `GET /api/health` probed with stored token
2. If 401: login overlay shown with token input
3. User enters token, tested against `/api/health`
4. On success: token saved to `localStorage`, overlay hidden, WS reconnects with token
5. Logout button clears token and reconnects

**Unit tests:**
- `getAuthToken()` / `setAuthToken()` / `clearAuthToken()` use `localStorage`
- `authHeaders()` includes Bearer token when present, omits when absent
- Login flow validates token before saving

---

## User Flow 11: Error Handling & Reconnection (Epic 4 M2)

**Scenario:** Network failures and daemon restarts.

1. WS disconnect shows reconnect banner
2. Fallback polling (`GET /api/state` every 5s) starts when WS is down
3. API errors show toast notifications (auto-dismiss 5s, with retry button)
4. 401 responses trigger login overlay
5. Request timeout (10s) shows "Request timed out" toast
6. On WS reconnect: banner hidden, polling stopped, state re-hydrated

**Unit tests:**
- `apiFetch()` timeout after 10s
- `apiFetch()` shows toast on non-ok response
- `apiFetch()` triggers login on 401
- `showToast()` creates element, auto-dismisses, supports retry callback

---

## Utility Functions

**Unit tests for pure functions:**
- `truncate(str, max)` — truncates with ellipsis, no-op when short
- `escapeHtml(str)` — escapes `&`, `<`, `>`
- `escapeAttr(str)` — escapes `&`, `"`, `<`, `>`
- `formatDuration(secs)` — seconds vs minutes formatting
- `ansi4Color(n)` / `ansi4BrightColor(n)` — color table lookups
- `ansi256Color(n)` — 4-bit, 8-bit, 216-cube, grayscale ranges

---

## Test File Location

```
crates/clhorde-web/static/tests/
├── app.test.js          # Unit tests for app.js pure functions & state logic
└── README.md            # How to run tests
```

## Running Tests

```bash
cd crates/clhorde-web/static
npx vitest run           # or: node --test tests/app.test.js (Node 22+ built-in runner)
```
