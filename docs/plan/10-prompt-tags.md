# Prompt grouping / tags

**Problem:** With 20+ prompts spanning different tasks, the flat list becomes hard to navigate. No way to categorize or group related prompts.

**Proposal:** Allow prompts to carry tags and support filtering/grouping by tag.

## Tagging

- In Insert mode, prefix prompt with `@tag` to tag it (e.g. `@frontend Fix the navbar`)
- Multiple tags supported: `@frontend @urgent Fix the navbar`
- Tags stripped from the prompt text sent to Claude, stored as metadata
- Tags shown as colored badges in the prompt list (e.g. `[frontend]` in a distinct color)

## Filtering by tag

- `/` filter mode already supports text search — extend it to recognize `@tag` as tag filter
- `/@frontend` shows only prompts tagged `@frontend`
- Combinable with text search: `/@frontend navbar`

## Optional: grouped display

- A toggle (e.g. `g` in Normal mode) to switch between flat list and grouped-by-tag view
- Groups shown as collapsible sections with tag header

## Persistence

- Tags stored in the prompt JSON file (new `tags: Vec<String>` field)
- Restored on startup with the prompt

**Files likely touched:** `prompt.rs` (tags field), `app.rs` (tag parsing, filter extension), `ui.rs` (tag badges, grouped view), `persistence.rs` (tags serialization)
