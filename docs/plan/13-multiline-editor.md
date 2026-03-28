# Multi-line prompt editor

**Problem:** The input bar is a single line. Complex prompts (multi-line instructions, code snippets, detailed requirements) are painful to compose in a 1-line text field.

**Proposal:** Improve the prompt editing experience with two options (not mutually exclusive):

## Option A: Expandable inline editor

- Input bar grows to 5-10 lines when in Insert mode (push main area up)
- `Shift+Enter` or `Alt+Enter` inserts a newline
- `Enter` still submits
- Basic cursor movement (Home/End, word jump with Ctrl+Left/Right)
- Line count indicator in the input bar border

## Option B: External editor (`$EDITOR`)

- A keybinding (e.g. `Ctrl+E` in Insert mode) opens `$EDITOR` with a temp file
- On editor close, the file contents become the prompt text
- Similar to how `git commit` works without `-m`
- Falls back to `vi` if `$EDITOR` is not set

## Recommendation

Implement both. Option A for quick multi-line edits, Option B for complex prompts. They complement each other.

**Files likely touched:** `app.rs` (multi-line input state, editor launch), `ui.rs` (expandable input bar rendering), `main.rs` (terminal teardown/restore for external editor)
