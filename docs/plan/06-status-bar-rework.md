# Status bar rework

**Problem:** The status bar uses 3 lines but feels static. It shows counters (active/queue/done/total) but doesn't convey activity or progress.

**Proposal:** Make the status bar more information-dense and alive:
- Mini progress bar showing worker pool utilization (e.g. `[████░░░░] 4/8 workers`)
- Show the currently selected prompt's status/id inline
- Optionally show elapsed wall-clock time since session start
- Reduce to 2 lines if possible, reclaiming vertical space

**Design constraints:**
- Must remain readable on 80-column terminals
- Color usage should stay consistent with existing mode indicators

**Files likely touched:** `ui.rs` (render_status_bar), possibly `app.rs` (track session start time)
