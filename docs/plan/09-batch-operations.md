# Batch operations on prompts

**Problem:** When managing many prompts, acting on them one-at-a-time is tedious. No way to retry 5 failed prompts at once, or kill all running workers.

**Proposal:** Add a visual selection mechanism and batch actions:

## Selection

- `Space` in Normal mode toggles selection on current prompt (visual marker, e.g. `[x]` or highlighted background)
- `v` enters visual/multi-select mode where j/k extends selection
- `V` selects all visible (filtered) prompts
- Selection persists across navigation, cleared on `Esc`

## Batch actions (when selection active)

- `r` — retry all selected completed/failed prompts
- `x` — kill all selected running prompts
- `d` — delete/remove all selected prompts (with confirmation)
- `m` — toggle mode on all selected pending prompts

## UI indicators

- Selected prompts show a distinct marker (e.g. `●` prefix or colored background)
- Status bar shows "N selected" count when selection is active
- Help bar updates to show batch-applicable actions

**Files likely touched:** `app.rs` (selection state, batch action handlers), `ui.rs` (selection rendering), `prompt.rs` (possibly batch status changes)
