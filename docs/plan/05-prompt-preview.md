# Prompt preview on selection

**Problem:** Prompt text is truncated to 30 chars in the list. Users can't tell what a prompt actually says without entering View mode.

**Proposal:** When a prompt is selected in the list, show the full prompt text in a dedicated area — either:
- A transient tooltip/popup near the selected item
- A line in the output panel header (below the title)
- A small preview pane between the list and the output viewer

**Scope:**
- Normal mode only (no extra UI in other modes)
- Full text, word-wrapped, capped at ~3 lines
- Disappears when navigating away or entering another mode

**Files likely touched:** `ui.rs` (render_prompt_list or render_output_viewer)
