# Visual feedback for queue reordering

**Problem:** When reordering pending prompts with `J`/`K` in Normal mode, the prompt silently swaps position. There's no visual indication that a move happened.

**Proposal:** Add brief visual feedback when a prompt is moved:
- Flash/highlight the moved prompt row for ~300ms (e.g. bright background then fade)
- Show a transient status message like "Moved #3 up" in the status bar or help bar
- Optionally show rank numbers for pending prompts (queue position) so users can see the ordering

**Scope:**
- Subtle highlight animation (2-3 frames)
- Transient message using existing status_message mechanism
- Only applies to pending prompts (non-pending are ignored by J/K already)

**Files likely touched:** `ui.rs` (render_prompt_list highlight logic), `app.rs` (track recently-moved prompt + timestamp)
