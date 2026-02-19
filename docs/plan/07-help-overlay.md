# Full keybinding help overlay (`?`)

**Problem:** The bottom help bar is a single compressed line. New users struggle to discover keybindings across 6 modes. The only alternative is reading CLAUDE.md or keymap.toml.

**Proposal:** Press `?` in Normal mode to open a full-screen overlay listing all keybindings grouped by mode:

```
 ┌─────────────────── Keybindings ───────────────────┐
 │                                                    │
 │  NORMAL              VIEW              INSERT      │
 │  i    insert mode     j/k  scroll       Enter send │
 │  j/k  navigate        s    interact     Esc   back │
 │  Enter view output    f    auto-scroll  C-w   WT   │
 │  s    interact        w    export       Tab   comp │
 │  m    toggle mode     x    kill                    │
 │  r    retry           q    back         FILTER     │
 │  R    resume                            Enter apply│
 │  J/K  reorder         INTERACT          Esc   clear│
 │  /    filter          Enter send                   │
 │  +/-  workers         Esc   back                   │
 │  q    quit                                         │
 │                                                    │
 │              Press ? or Esc to close               │
 └────────────────────────────────────────────────────┘
```

**Scope:**
- New app mode `Help` or render as a popup overlay (no mode change needed)
- Content auto-generated from the loaded keymap (respects custom bindings)
- Scrollable if terminal is too short
- `?` or `Esc` to dismiss

**Files likely touched:** `ui.rs` (new render_help_overlay), `app.rs` (help state or mode), `keymap.rs` (structured export of bindings)
