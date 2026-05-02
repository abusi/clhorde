# OpenSpec Scheduler & Workflow Pivot

## Overview

Pivot clhorde from "TUI for running multiple Claude Code instances in parallel" to **a runtime platform for spec-driven AI development workflows**. Add a new crate `clhorde-scheduler` that consumes [OpenSpec](https://openspec.dev/) changes and orchestrates their implementation as a DAG of dependent prompts dispatched to `clhorded`, while the existing TUI keeps its role as an interactive workshop for *exploring* ideas and drafting new specs.

The pivot is additive: existing prompt-management features stay as-is. The scheduler builds **on top of** new low-level primitives (prompt dependencies, shared worktrees) that are useful even outside the OpenSpec context.

## Status (2026-05-02)

Tracked on branch `feat/prompt-dependencies`.

| Phase | Status | Notes |
|-------|--------|-------|
| 0.1 Prompt dependencies | ✅ shipped | commit `ea2e405`, 10 new tests |
| 0.2 Shared worktrees | ✅ shipped | commit `c1e7309`, 14 new tests |
| 0.3 Generic prompt annotations | ✅ shipped | 9 new tests; daemon stays workflow-agnostic |
| 1   tasks.md parser + DAG | ✅ shipped | new `clhorde-scheduler` crate (lib-only); 53 new tests |
| 2   Scheduler execution loop + openspec FS detection | ⏳ next | |
| 3   `clhorde-cli flow` wrappers | ⏳ pending | |
| 4   TUI restructure (tabs) | ⏳ pending | |
| 5   Advanced / web | ⏳ pending | |

Workspace tests: **459 passing**, none ignored.

## Vision

Two complementary modes of working with Claude:

```
┌──────────────────────────────────┐    ┌──────────────────────────────────┐
│ EXPLORATION (human ↔ Claude)     │    │ EXECUTION (scheduler ↔ Claude×N) │
│                                  │    │                                  │
│  Long-lived interactive PTY      │    │  DAG of dependent prompts        │
│  Iterate on proposal/design      │ ─► │  Shared worktree per workflow    │
│  Produces openspec/changes/<X>/  │    │  Ticks tasks.md, archives        │
│  TUI = workshop                  │    │  Scheduler = conductor           │
└──────────────────────────────────┘    └──────────────────────────────────┘
              ↑                                      ↑
       Tab "Drafts"                           Tab "Workflows"
```

The user explores N ideas in parallel in the TUI. When an idea matures into a queued OpenSpec change, the scheduler picks it up and implements it through a series of coordinated prompts while the user keeps exploring the next idea. Both modes share the same worker pool (bounded by `max_workers`).

## Background: OpenSpec primer

OpenSpec is a file-based spec-driven development framework. It produces a directory tree that AI agents read and edit:

```
openspec/
├── specs/<domain>/spec.md          ← current system behavior
└── changes/<change-name>/
    ├── proposal.md                 ← rationale and scope
    ├── design.md                   ← technical approach
    ├── specs/<domain>/spec.md      ← delta (ADDED/MODIFIED/REMOVED)
    └── tasks.md                    ← hierarchical checkbox list
```

`tasks.md` uses numbered sections with decimal sub-items:

```markdown
## 1. Theme Infrastructure
- [ ] 1.1 Create ThemeContext
- [ ] 1.2 Add CSS variables

## 2. UI Components
- [ ] 2.1 Build toggle component
```

OpenSpec itself ships no runtime — it relies on the AI agent (typically Claude Code) to read these files and act on them sequentially within a single conversation. The lifecycle is `propose → apply → archive`, driven by slash-commands the agent invokes (`/opsx:propose`, `/opsx:apply`, etc.).

**Key gap clhorde fills:** OpenSpec describes *what* to do but has no engine to drive *how* — no parallelism, no checkpointing, no recovery from partial failures, no separation between exploration and execution. clhorde already runs N Claude instances. Bridging the two yields a workflow engine for spec-driven development.

## Conceptual Model

A change in `openspec/changes/<name>/` moves through a state machine:

```
[Drafted]   ──user reviews/refines──>  [Drafted]      (loops, while exploring)
[Drafted]   ──queue──────────────────> [Queued]
[Queued]    ──scheduler picks up─────> [Implementing] (apply phase)
[Implementing] ─all sections done────> [Verifying]
[Verifying] ──ok─────────────────────> [Archiving]
[Archiving] ─done────────────────────> [Archived]
[*]         ──user cancels───────────> [Drafted | Cancelled]
[*]         ──fails──────────────────> [Failed]      (with retry options)
```

The user controls the boundary between exploration and execution. The scheduler **never auto-promotes** a draft.

## Architecture

Three layers of responsibility:

```
┌─────────────────────────────────────────────────────────┐
│ clhorde-scheduler (new crate)                           │
│  - watches openspec/changes/*/.clhorde-ready            │
│  - parses tasks.md, builds DAG                          │
│  - templates prompts per phase                          │
│  - tracks workflow state (status, retry, archive)       │
│  - reads tasks.md after each worker to detect ticks     │
└────────────────┬────────────────────────────────────────┘
                 │ IPC (existing ClientRequest/DaemonEvent)
                 │ + new prompt-dependency semantics
                 ▼
┌─────────────────────────────────────────────────────────┐
│ clhorded (extended, but workflow-agnostic)              │
│  - prompts with dependencies (depends_on: Vec<UUID>)    │
│  - prompts with shared worktrees (worktree_id)          │
│  - new PromptStatus::Blocked                            │
│  - generic prompt annotations (key/value bag)           │
└────────────────┬────────────────────────────────────────┘
                 │ spawn
                 ▼
            claude (PTY/oneshot)
```

The scheduler runs as **a long-lived client of the daemon** (separate binary, but uses the same Unix socket protocol). It does *not* extend the daemon directly — that keeps `clhorded` agnostic of OpenSpec and lets us swap in other workflow sources later (Linear issues, GitHub Issues, custom YAML, etc.).

## Handoff: `.clhorde-ready` marker

A change is picked up by the scheduler if and only if `openspec/changes/<name>/.clhorde-ready` exists. This file is the explicit handoff contract.

**Why a marker file:**
- Git-trackable if the team wants to share the queue.
- Detectable via `notify`/inotify without polluting OpenSpec artifacts.
- Trivially reversible (`rm` = back to draft).
- Can carry optional metadata (priority, target branch, inter-change deps).

**Format** (YAML, all fields optional):

```yaml
# openspec/changes/<name>/.clhorde-ready
priority: 5                    # higher runs first when workers free up
worktree_branch: feature/oauth # default: detached HEAD
depends_on: [add-base-auth]    # other change names that must be Archived first
parallel_sections: [3]         # section numbers safe to run in parallel
max_section_retries: 2
```

**How the marker gets written:**
- TUI: `Q` keybinding on a draft.
- CLI: `clhorde-cli flow queue <change-name> [--priority N]`.
- Claude Code itself: at the end of `/opsx:propose`, the agent can write the marker if the user confirmed.

**Removal:** the scheduler removes the marker after archiving (the change leaves the queue naturally) or on `clhorde-cli flow unqueue <name>`.

## New Crate: `clhorde-scheduler`

```
crates/
└── clhorde-scheduler/
    ├── Cargo.toml
    └── src/
        ├── main.rs              # binary entry, CLI args, event loop
        ├── cli.rs               # subcommand dispatch (queue, status, retry, …)
        ├── daemon_client.rs     # long-lived async IPC client (subscribe + send)
        ├── workflow.rs          # Workflow type, state machine
        ├── persistence.rs       # ~/.local/share/clhorde/workflows/*.json
        ├── openspec/
        │   ├── mod.rs
        │   ├── discovery.rs     # scan openspec/changes/*/, detect .clhorde-ready
        │   ├── tasks_parser.rs  # parse tasks.md → TaskGraph
        │   ├── dag.rs           # DAG builder + cycle check
        │   └── annotations.rs   # parse <!-- depends: ... --> comments
        ├── templates.rs         # prompt template engine (tera)
        ├── watcher.rs           # notify-based FS watcher
        └── orchestrator.rs      # binds it all: queue → dispatch → track → archive
```

### Binary: `clhorde-scheduler`

| Subcommand | Purpose |
|------------|---------|
| `daemon` | Run as long-lived watcher (default if no subcommand and `--watch` set) |
| `propose <idea>` | Spawn one PTY prompt running `/opsx:propose <idea>` and wait for the change directory to appear |
| `queue <name>` | Write `.clhorde-ready` |
| `unqueue <name>` | Remove `.clhorde-ready` |
| `status [name]` | Show workflow state(s) |
| `apply <name>` | Force pick-up now (one-shot, even without `--watch`) |
| `archive <name>` | Run `/opsx:archive` for a verified change |
| `retry <name> --section N` | Re-dispatch a failed section |
| `cancel <name>` | Kill running prompts of this workflow, mark Cancelled |
| `drafts` | List unqueued changes |
| `templates path` / `edit` | Manage prompt templates |

The scheduler is intended to run as a background process (started by the user, akin to `clhorded`). It can also operate as a one-shot CLI for scripting.

### Subcommand integration via `clhorde-cli`

To keep one entrypoint for users, mirror the scheduler subcommands as `clhorde-cli flow <subcommand>`. `clhorde-cli flow daemon` simply execs `clhorde-scheduler daemon`. This means users learn one CLI; the scheduler binary stays usable independently for scripting.

## Phase 0 — Core primitives in `clhorded`

These primitives are useful **independently of OpenSpec** and unblock everything else.

### 0.1 Prompt dependencies — ✅ shipped (`ea2e405`)

Delivered:
- `Prompt::depends_on: Vec<String>` (UUIDs of other prompts).
- New `PromptStatus::Blocked` 🔒 — set on submit when deps aren't all `Completed`, transitioned to `Pending` by `unblock_dependents()` when the last dep finishes.
- `Orchestrator::next_pending_prompt_index` filters Pending only; Blocked prompts are skipped and re-evaluated after each `WorkerFinished`.
- A dependent stays Blocked indefinitely if its dep is `Failed` — no auto-cascade.
- Cycle detection (`would_create_cycle`, DFS) on `SetDependencies`. Submit-time cycles are impossible (the new prompt has no incoming edges yet).
- IPC: `ClientRequest::SubmitPrompt { …, depends_on: Vec<usize> }` + new `SetDependencies { prompt_id, depends_on }`. The daemon resolves client-facing IDs to internal UUIDs and returns `DaemonEvent::Error` on unknown IDs.
- `PromptInfo` exposes `depends_on: Vec<String>` and `blocked_by: Vec<String>` (computed unmet deps).
- Persistence: `PromptFile.depends_on` with `#[serde(default)]`. Back-compat test in place.
- Deletion purges the deleted UUID from any other prompt's `depends_on` and re-evaluates blocked dependents.

Plumbing:
- `clhorde-cli submit "..." --depends-on 1,2,3` (alias `--after`).
- `clhorde-web POST /api/prompts { "depends_on": [1, 2] }`.
- TUI sends an empty `depends_on` for now; UI surface deferred to Phase 4.

**Decision: ID resolution at the boundary.** Clients submit prompts referencing IDs (`usize`) for ergonomics; the daemon resolves them to UUIDs at submit time before persisting. Internally everything is UUIDs.

### 0.2 Shared worktrees — ✅ shipped (`c1e7309`)

Delivered:
- `Prompt::worktree_id: Option<String>`, persisted in `PromptOptions` with serde defaults.
- `clhorde-core::worktree` exposes `create_worktree_named(repo, suffix)` with a sanitized suffix (`[A-Za-z0-9._-]`, runs collapsed, empty falls back to `wt`); `create_worktree(prompt_id)` is a thin wrapper to keep legacy per-prompt naming intact.
- `dispatch_workers`, when `worktree=true && worktree_id = Some(id)`:
  1. reuses a sibling's `worktree_path` if already resolved (and broadcasts the propagation),
  2. skips dispatch (`continue`) if `worktree_id_creation_in_flight(id)` — retried on next `WorktreeCreated`,
  3. otherwise spawns creation with `<repo>-wt-<sanitized_id>`.
- `WorktreeCreated` propagates the resolved path to all sibling prompts of the same `worktree_id`. On error, every pending sibling without a path is marked `Failed`; siblings that already had a path keep theirs.
- Refcounted cleanup:
  - `maybe_cleanup_worktree` (auto) checks `shared_worktree_active_count(id)` and skips removal while any sibling is in `Pending/Blocked/Running/Idle`.
  - `clean_worktrees` (manual) deduplicates by `worktree_id` and respects the same refcount.
  - When the last sibling reaches a terminal state, the path is cleared from all siblings and the git worktree is removed in a background thread.
- `RetryPrompt` preserves `worktree_id` so the retry rejoins the same shared worktree.
- IPC: `ClientRequest::SubmitPrompt` and `PromptInfo` carry `worktree_id: Option<String>`.

Plumbing:
- `clhorde-cli submit "..." --worktree --worktree-id <id>`.
- `clhorde-web POST /api/prompts { "worktree": true, "worktree_id": "flow-42" }`.
- TUI passes `None` for now (the scheduler will be the primary producer of shared ids).

### 0.3 Generic prompt annotations — ✅ shipped

**Why redefined:** the original 0.3 ("auto-link prompts ↔ openspec changes") would have hard-coded the OpenSpec taxonomy into `clhorded`, which contradicts the architectural principle "scheduler is a *client* of the daemon; daemon stays workflow-agnostic" (see Risks). We split the concern: the daemon gets a generic key/value surface, and the OpenSpec-specific FS detection moves into the scheduler in Phase 2.

What ships in 0.3:
- `Prompt::annotations: BTreeMap<String, serde_json::Value>` — opaque key/value bag the daemon stores, persists, and broadcasts but does **not** interpret. `BTreeMap` so the wire output is order-stable.
- `PromptFile.annotations` and `PromptInfo.annotations`, both `#[serde(default)]` for back-compat.
- New IPC: `ClientRequest::SetAnnotation { prompt_id, key, value }`. `value: Value::Null` removes the key (single endpoint covers set/remove). Unknown `prompt_id` returns `DaemonEvent::Error`.
- Annotation writes broadcast `DaemonEvent::PromptUpdated(PromptInfo)` so all subscribers see the new state.
- Persistence: annotations round-trip through `~/.local/share/clhorde/prompts/<uuid>.json`.
- No special handling — annotations stay attached across `RetryPrompt` only by *not* being copied (retry creates a fresh prompt). Delete purges them with the prompt.

What this enables:
- Phase 2's scheduler subscribes, snapshots `<effective_cwd>/openspec/changes/` itself on `WorkerStarted` and again on `WorkerFinished`, then writes `SetAnnotation { key: "openspec.affected_changes", value: ["add-oauth", ...] }`.
- Phase 4's TUI reads the same annotation key to suggest "queue this change?".
- Future workflow sources (Linear, GitHub Issues, custom YAML) use their own keys without touching the daemon.

The race window between `WorkerStarted` broadcast and the scheduler taking its first snapshot is acceptable in practice (Claude Code takes seconds to do anything FS-visible). If it ever bites, we add an explicit "watch_dirs" parameter on submit — still as a generic primitive — without baking OpenSpec into the daemon.

## Phase 1 — Tasks.md parser & DAG builder — ✅ shipped

Delivered (lib-only crate `clhorde-scheduler`, no binary yet — Phase 2 adds it):

- `clhorde-scheduler::openspec::tasks_parser`
  - `parse(&str) -> TaskGraph`. Line-based parser (no `pulldown-cmark` — the constrained subset is simpler hand-rolled and gives exact line numbers for free).
  - Recognizes `## N. Title`, `### N.M Title`, and list markers `- `/`* `/`+ ` followed by `[ ]`/`[x]`/`[X]`.
  - Tracks fenced code blocks (` ``` ` and `~~~`) so task-like lines inside code samples are ignored.
  - Strips inline `<!-- ... -->` from titles and task text but preserves the verbatim `source_line` for the annotations pass.
  - Tolerates prose between items, free-form tasks (empty id), and `H1` documentation lines.
  - Drops orphan tasks that appear before any heading and headings without a leading dotted id.
- `clhorde-scheduler::openspec::annotations`
  - `annotate(TaskGraph) -> Vec<AnnotatedSection>`. Walks the tree once and re-scans each `source_line` for `<!-- clhorde: ... -->` comments.
  - Recognized keys: `depends`, `parallel-with`, `granularity` (`section`|`task`|`phase`), `prompt-template` on sections; `needs` on tasks.
  - Multiple directives separated by `;`; multi-value lists by `,` or whitespace.
  - Unknown keys are silently ignored (forward-compat); malformed comments don't block the workflow.
- `clhorde-scheduler::openspec::dag`
  - `build(&[AnnotatedSection]) -> Result<Dag, DagError>`.
  - Granularity (Section / Task / Phase) is picked from the first section that sets it; default is Section.
  - Section default policy: section *i* depends on section *i-1* in source order. `depends` annotation **replaces** the default. `parallel-with` is recorded as a hint without removing edges.
  - Task granularity: tasks default to sequential within their section, reset across sections; `needs` annotations override.
  - Phase granularity: collapses everything to one `apply` node.
  - Three-color DFS cycle detection; reports the back-edge path on `DagError::Cycle`. Unknown ids surface as `DagError::UnknownRef { kind }`. Empty input → `DagError::Empty`.

53 unit tests cover all of the above (parser, annotations, DAG, error paths, cycle shapes).

### Original design notes (kept for reference)

```rust
struct TaskGraph {
    sections: Vec<Section>,
    annotations: GraphAnnotations,
}

struct Section {
    id: String,           // "1", "2.3"
    title: String,        // "Theme Infrastructure"
    parent: Option<String>,
    items: Vec<TaskItem>,
    deps: Vec<String>,    // explicit from annotations
    parallel_with: Vec<String>,
}

struct TaskItem {
    id: String,           // "1.2"
    text: String,
    done: bool,           // checkbox state
    line_range: (usize, usize), // for re-writing
}
```

The shipped types diverge slightly: `Section` and `TaskItem` carry a `source_line: String` field used by the annotations pass instead of an inline `deps`/`parallel_with`; annotations live on the post-walk `AnnotatedSection`/`AnnotatedTask` envelopes. `line_range` is currently always single-line — multi-line task continuation can be added when a real workflow needs it.

### Annotations

Optional HTML comments adjacent to a section header or task line:

```markdown
## 3. Tests <!-- clhorde: depends 2; parallel-with 4 -->
- [ ] 3.1 Unit tests
- [ ] 3.2 E2E <!-- clhorde: needs 2.1 -->
```

Recognized keys: `depends`, `parallel-with`, `needs`, `granularity` (`section`|`task`), `prompt-template` (override).

### DAG construction

Default policy: section `N` depends on section `N-1` (sequential). Annotations override. Cycle check via DFS; reject the workflow with a clear error.

Granularity decides the prompt unit:
- `section` (default): one prompt per leaf section.
- `task`: one prompt per `- [ ]` item — only if explicitly set, since context overhead is steep.
- `phase`: one prompt for the entire `apply` — fallback to OpenSpec's native flow.

## Phase 2 — Prompt templates

A directory `~/.config/clhorde/scheduler/templates/` with [Tera](https://keats.github.io/tera/) templates:

```
templates/
├── propose.md         # vars: {{idea}}, {{repo_name}}
├── apply-section.md   # vars: {{change_name}}, {{section_id}}, {{section_title}}, {{tasks_block}}, {{change_dir}}
├── verify.md          # vars: {{change_name}}
└── archive.md         # vars: {{change_name}}
```

Default `apply-section.md`:

```
You are working on OpenSpec change `{{change_name}}`.

Read the proposal, design, and specs in:
  {{change_dir}}/proposal.md
  {{change_dir}}/design.md
  {{change_dir}}/specs/

Your job: complete section {{section_id}} ({{section_title}}) of {{change_dir}}/tasks.md.

Tasks to implement:
{{tasks_block}}

When each task is done, edit {{change_dir}}/tasks.md and change `- [ ]` to `- [x]`
for that exact line. Do not start any other section.

Run any relevant tests after the section. If tests fail, fix and re-run.
Stop when all tasks of this section are checked.
```

Per-project override: `openspec/.clhorde-scheduler/templates/` takes precedence over the user-level one.

## Phase 3 — Workflow execution loop

The scheduler's main loop, in pseudocode:

```rust
loop {
    select! {
        // 1. New / removed marker files
        ev = fs_watcher.recv() => match ev {
            Created(.clhorde-ready) => start_workflow(change_name),
            Removed(.clhorde-ready) => maybe_cancel(change_name),
            Modified(tasks.md)      => refresh_workflow_state(change_name),
        },

        // 2. Daemon events (we are subscribed)
        ev = daemon.recv() => match ev {
            WorkerFinished { prompt_id, exit_code } => {
                if let Some(workflow) = lookup_by_prompt(prompt_id) {
                    workflow.on_prompt_done(prompt_id, exit_code);
                    // re-read tasks.md, advance phase, dispatch next prompts
                }
            }
            // ...
        },

        // 3. CLI requests on a control socket (status, cancel, retry)
        cmd = ctrl.recv() => handle_ctrl(cmd),
    }
}
```

Dispatching a workflow phase = computing the set of currently-runnable nodes from the DAG, templating their prompts, and submitting them with `depends_on` filled in by the previous-section UUIDs and `worktree_id` set to the workflow id.

The scheduler **trusts `tasks.md` as the source of truth for completion**. After each `WorkerFinished`, it re-parses `tasks.md`. A section is "done" when all its `- [ ]` are now `- [x]`. If the worker exited 0 but tasks weren't all checked, the scheduler can either retry or pause and ask (configurable, default: pause).

## Phase 4 — TUI restructure

Introduce a top-level tab bar above the existing list/output panels:

```
┌──── [P]rompts  [D]rafts  [W]orkflows ────────────────────┐
│                                                          │
│  Prompts:    (existing view, unchanged behavior)         │
│                                                          │
│  Drafts:     scan of openspec/changes/* without .ready   │
│    > add-oauth-login   (drafted, last edit 2m ago)       │
│        Q  queue this change                              │
│        E  continue exploring (relaunch interactive)      │
│        R  open proposal/design in $PAGER                 │
│                                                          │
│  Workflows:  queued + active + archived                  │
│    > add-oauth-login   apply 3/7 sections   2 active     │
│    > refactor-auth-mw  verify   waiting                  │
│        Enter  show DAG + per-section prompt status       │
│        P      pause   X cancel   T retry section         │
└──────────────────────────────────────────────────────────┘
```

Implementation:
- A new enum `RootView { Prompts, Drafts, Workflows }` in `app.rs`.
- The tab is switchable by digit (`1`/`2`/`3`) or letter; current `AppMode` becomes scoped to a view where relevant.
- `Drafts` and `Workflows` data come from the scheduler — TUI subscribes to a control endpoint exposed by the scheduler (additional Unix socket: `~/.local/share/clhorde/scheduler.sock`).
- Status banner stays global (workers, max_workers, etc.).

The web UI mirrors this with `/api/workflows`, `/api/drafts` routes proxied through the scheduler.

## Phase 5 — Advanced (deferred)

- **Disjoint-files analysis**: refuse `parallel-with` annotations when sections touch overlapping files (cheap heuristic via grep over `proposal.md` / `design.md`).
- **Inter-workflow deps**: `depends_on: [other-change-name]` in `.clhorde-ready` defers pickup until the other change is `Archived`.
- **Auto-retry policies** per phase (apply: 2, verify: 1, archive: 0; configurable).
- **Hooks**: `post-archive: gh pr create --title "{{change_name}}"`.
- **Multi-repo**: a scheduler instance can watch several repos.

## Open questions

1. **Scheduler as separate daemon vs. embedded in `clhorded`.** The plan goes with separate. Risk: two daemons to manage. Mitigation: `clhorde-cli flow daemon` wraps it; eventual `systemd --user` units in `docs/`.
2. **Marker file format vs. status field.** YAML in `.clhorde-ready` is more flexible than a flag. We accept the small parse cost.
3. **Cycle handling in workflows that depend on each other.** Reject at queue time.
4. **What if a workflow's `tasks.md` is edited mid-flight?** Re-parse on `notify` Modified. If an in-progress section's tasks change, log a warning but trust the new state on next dispatch. Detail to refine.
5. **Concurrency between scheduler and human-in-TUI editing the same `openspec/` dir.** Both write `[x]` (Claude Code does for the scheduler; human might in TUI). Last write wins; we accept this — `tasks.md` is conflict-tolerant.
6. **`.clhorde-ready` in git or `.gitignore`?** User's choice. Document both patterns.

## Out of scope (initial release)

- Web UI for workflows (Phase 4 only ships the TUI; web comes later).
- Spec-Kit (GitHub) compatibility — same architecture would suit it, but defer.
- Distributed scheduler across machines.
- Auth/permissions on the scheduler control socket (relies on filesystem perms, like the daemon).
- Cost/token budgets per workflow.

## Phased delivery

| Phase | Status | Scope | Why now |
|-------|--------|-------|---------|
| **0.1** | ✅ `ea2e405` | Prompt dependencies + `Blocked` status | Foundational. Useful even without the scheduler. |
| **0.2** | ✅ `c1e7309` | Shared worktrees via `worktree_id` + refcounted cleanup | Required for sequential workflow steps to share branch state. |
| **0.3** | ✅ shipped | Generic `Prompt::annotations` + `SetAnnotation` IPC | Workflow-agnostic primitive. OpenSpec FS detection moves to Phase 2. |
| **1**   | ✅ shipped | `clhorde-scheduler` crate skeleton + tasks.md parser + DAG builder | Core algorithm; testable in isolation without IPC. |
| **2**   | ⏳ next | Templates + execution loop + watcher + persistence + openspec/changes snapshot/diff in scheduler | Minimum viable scheduler — runs on the CLI. Owns `openspec.affected_changes` annotation. |
| **3**   | ⏳ pending | `clhorde-cli flow` wrappers + `propose`/`queue`/`status`/`apply`/`archive` | Usable by humans end-to-end. |
| **4**   | ⏳ pending | TUI tabs (`Drafts`, `Workflows`) + scheduler control socket | First-class UX. |
| **5**   | ⏳ pending | Web routes + advanced (parallel safety, hooks, multi-repo) | Polish. |

Each phase is independently shippable. Phase 0 alone is already a feature win; Phase 2 already produces value for users who want a scriptable workflow runner.

## Risks

- **OpenSpec is young and may evolve.** Keep the scheduler logic behind a thin `openspec::` module so we can absorb format changes (e.g. new `tasks.md` syntax) in one place. Don't lean on undocumented internals.
- **Spec-Kit (GitHub) overlap.** Same architecture would serve it; design `clhorde-scheduler` so the OpenSpec-specific bits are pluggable (`trait WorkflowSource`).
- **Complexity creep.** The scheduler should remain a *client* of `clhorded`. Resist adding workflow concepts to the daemon beyond Phase 0 primitives.
- **Worktree race conditions.** Shared worktrees mean concurrent prompts could clash. Default sequential within a workflow's `apply` phase; `parallel-with` is an explicit opt-in.
- **Auto-linking false positives.** A prompt that touches `openspec/specs/` (not `changes/`) might pollute the `openspec.affected_changes` annotation. The scheduler (Phase 2) restricts its scan to `openspec/changes/`.

## Acceptance criteria for the pivot

A user should be able to run, end-to-end:

```bash
# Explore in TUI: launch interactive prompt that drafts the change.
clhorde
# (in TUI) press i, type: /opsx:propose Add OAuth login
# Claude Code creates openspec/changes/add-oauth-login/

# Queue it.
clhorde-cli flow queue add-oauth-login

# Background scheduler is already running; it picks up automatically.
clhorde-cli flow status add-oauth-login
# add-oauth-login: applying, section 2/5, prompts 47, 48 active

# Continue exploring something else while the above runs.
clhorde
# (in TUI) press 'P', new prompt: /opsx:propose Add billing dashboard
# Both flows progress in parallel within max_workers.

# Eventually:
clhorde-cli flow status add-oauth-login
# add-oauth-login: archived, took 18m, 5 sections, 3 retries
```

When this works, the pivot is delivered.
