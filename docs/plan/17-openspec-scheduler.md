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
| 2.1 Binary skeleton + daemon_client | ✅ shipped | clap CLI, long-lived IPC client, reconnect loop; 17 new tests |
| 2.2 Discovery + workflow types + persistence | ✅ shipped | TOML marker, state machine, JSON store; 32 new tests |
| 2.3 FS watcher + state machine | ✅ shipped | `notify_debouncer_full`, orchestrator + reconcile; 26 new tests |
| 2.4 Templates + dispatch | ✅ shipped | Tera engine, DAG dispatch, apply→verify→archive lifecycle; 35 new tests |
| 2.5 openspec/changes/ snapshot in scheduler | ✅ shipped | content-hash snapshot/diff + `SetAnnotation` writes on every observed worker; 21 new tests |
| 2.6 CLI subcommands wired to daemon | ✅ shipped | every stub replaced; one-shot `Ping`/`Pong` fence; 23 new tests |
| 3   `clhorde-cli flow` wrappers + scheduler control socket | ✅ shipped | `~/.local/share/clhorde/scheduler.sock`, live remote-control; 25 new tests |
| 4.1 TUI tabs (foundation, read-only) | ✅ shipped | `RootView`, tab bar, scheduler-control client, polled Drafts/Workflows panes; 13 new tests |
| 4.2 TUI tabs (actions: Q/X/T/E/R) | ✅ shipped | `Queue` control variant + `root` on `Status`; pager suspend/restore; retry-section inline prompt; 24 new tests |
| 4.3 TUI workflow detail view + freshness badges | ✅ shipped | `Detail` control variant + `Orchestrator::detail`; Enter zoom + auto-refresh; staleness suffix on titles; 22 new tests. Push-based subscribe deferred to Phase 5. |
| 5.1 Push-based scheduler Subscribe | ✅ shipped | Broadcast-backed `Subscribe` request + `SchedulerEvent` stream replaces TUI 2s Status polling; 20 new tests |
| 5.2 Web routes + dashboard tabs | ✅ shipped | `SchedulerBridge` + 5 REST routes + WS fan-out + Drafts/Workflows SPA tabs; 14 new tests |
| 5.x Advanced (parallel safety, inter-workflow deps, hooks, multi-repo, …) | ⏳ pending | Polish. |

Workspace tests: **738 passing**, none ignored.

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

## Phase 2 — Scheduler runtime

The full execution loop, broken into shippable slices. Each sub-phase ends with a green test suite and its own commit so we can land it incrementally.

### 2.1 Binary skeleton + daemon client — ✅ shipped (`9382e38`)

Delivered:
- `clhorde-scheduler::cli` — clap-derive subcommand types (`daemon`, `apply`, `archive`, `cancel`, `drafts`, `propose`, `queue`, `unqueue`, `retry`, `status`, `templates path|edit`). Global `--log` flag for tracing filter override. Pure parsing, no IPC — fully unit-testable.
- `clhorde-scheduler::daemon_client` — long-lived async IPC connector mirroring the TUI's pattern. Reader and writer drive independent `tokio::spawn` tasks fed by `mpsc` channels; PTY frames are dropped (the scheduler only reads structured events), malformed JSON is logged and skipped without disconnecting. `spawn_loops` exposed for in-memory testing via `tokio::io::duplex`.
- `main.rs` — runtime, tracing, and a `daemon` subcommand that subscribes and idles until SIGINT, reconnecting with backoff when the daemon goes away. All other subcommands stub out (return exit code 2) until 2.6.
- 17 new tests: 12 CLI parsing scenarios + 5 daemon-client lifecycle (event delivery, disconnect, PTY filter, malformed JSON resilience, state-snapshot round-trip).

Latent issue surfaced here for follow-up: `ClientRequest::SetMaxWorkers(usize)` is a newtype variant containing a primitive, which `serde_json` cannot serialize under `#[serde(tag = "type")]`. Tracked as a separate fix; doesn't block the scheduler since we don't issue that request from here.

### 2.2 Discovery + workflow types + persistence — ✅ shipped

Delivered:
- `openspec::discovery::scan(root)` walks `<root>/openspec/changes/*/` and classifies each entry as `Drafted` or `Queued(MarkerMetadata)`. The marker is plain TOML (`priority`, `depends_on`, `worktree_branch`, `parallel_sections`, `max_section_retries`); unknown fields are ignored, malformed markers degrade to `Drafted` (with a warning), and dotfile entries (`.archive/`) are skipped.
- `workflow::Workflow` with a pure state machine for `Drafted → Queued → Implementing → Verifying → Archiving → Archived` plus `Failed { reason }` / `Cancelled`. `cancel`/`fail` are valid from any non-terminal state; every illegal transition returns `TransitionError { from, attempted }`.
- `persistence::WorkflowStore` saves to `~/.local/share/clhorde/workflows/<name>.json` via tempfile + atomic rename. Workflow names are validated (rejects path traversal and dotfiles). Every persisted field carries `#[serde(default)]`; the back-compat test loads a minimal historical record cleanly.
- The `dag` is intentionally not persisted — Phase 2.4 rebuilds it from `tasks.md`.

32 new tests (workspace 476 → 508).

**Marker format:** TOML rather than YAML — already in workspace deps, no new dependency, and the marker payload is small enough that human readability isn't impacted.

### 2.3 FS watcher + state machine wiring — ✅ shipped

Delivered:
- `watcher::spawn` runs `notify_debouncer_full` against `<root>/openspec/changes/`, classifies each path through pure `classify_path` / `classify_event` helpers, and forwards a stream of `FsEvent { MarkerCreated | MarkerRemoved | TasksModified }` over an `mpsc::UnboundedSender`. Marker create-vs-remove is decided by re-checking presence on disk, which collapses platform-dependent `notify` event semantics into the only two states the orchestrator cares about.
- `orchestrator::Orchestrator` owns the in-memory workflow map plus the `WorkflowStore`. `handle_event` mutates state and persists; `reconcile` reads the store + scans the FS so markers that appeared (or disappeared) while the scheduler was offline are folded back into the active set. Edge cases: marker re-creation on a `Queued` workflow refreshes metadata only; marker removal on a running workflow → `cancel`; missing `tasks.md` clears the parsed cache silently.
- The `daemon` subcommand now calls `reconcile` on startup, spawns the watcher, and applies events via the orchestrator while the daemon connection reconnects in the background. Watcher failures are non-fatal — the daemon stays up so the user can still run `apply` manually once Phase 2.6 lands.
- Phase 2.3 stops short of dispatching prompts; the parsed `tasks.md` graph is cached on `Orchestrator` for Phase 2.4 to consume.

26 new tests (workspace 508 → 534), driving the orchestrator with synthetic `FsEvent`s plus one live-watcher smoke test that creates a `.clhorde-ready` and asserts the event surfaces within 2s.

### 2.4 Templates + prompt dispatch — ✅ shipped

Delivered:
- `templates.rs` — Tera engine with four built-in templates (`propose`, `apply-section`, `verify`, `archive`) included via `include_str!`. Override resolution: per-project (`<root>/openspec/.clhorde-scheduler/templates/`) beats user (`~/.config/clhorde/scheduler/templates/`) beats built-in. Malformed or unreadable overrides log a warning and fall through to the next layer; only known template names are renderable so a stray file with a typo can't accidentally route a prompt.
- `dispatch.rs` — pure decision helpers: `next_runnable_nodes(dag, completed, dispatched)`, `is_section_done`, `is_task_done`, `is_node_done`. Granularity-agnostic; the orchestrator stays small because every "what fires next" decision lives here.
- `orchestrator.rs` extension — outbound `mpsc::UnboundedSender<ClientRequest>`, `WorkflowRuntime` with the apply DAG plus per-node dispatch bookkeeping, and a single `try_advance(name)` entry point invoked after every FS or daemon event. Phases:
  - `Queued` + parsed tasks → build DAG, transition to `Implementing`, dispatch initial wave with `worktree=true`, `worktree_id = <workflow>`, and `depends_on` populated from predecessor prompt ids that arrived via `PromptAdded`.
  - `Implementing` + each `WorkerFinished` → re-parse `tasks.md`, mark the node complete iff its section/task boxes are all ticked. Worker exit ≠ 0 fails the workflow; exit 0 with unchecked boxes also fails (the "pause" default in the original plan, surfaced as `Failed { reason }` so it shows up in `status`).
  - `Implementing` complete → `Verifying`, then `Archiving`, then `Archived`. Verify/archive are single-prompt phases that re-use the same template engine and tag scheme.
- Tag-based correlation: every dispatched prompt gets `clhorde-scheduler/wf=<name>/phase=<phase>[/node=<id>]` so `PromptAdded` and `WorkerFinished` events route back to the right workflow without touching the daemon.
- `main.rs` — outbound channel forwarded to the daemon socket inside the existing select! loop; daemon events also feed `Orchestrator::handle_daemon_event`. Watcher and reconcile logic from 2.3 are unchanged.

35 new tests (workspace 534 → 569): template rendering + override layering, pure DAG dispatch (initial wave, fan-in, parallel branches), and end-to-end orchestrator scenarios driving apply → verify → archive with synthetic `PromptAdded` / `WorkerFinished` events through the outbound channel.

In-flight workflows that survive a scheduler crash currently re-dispatch from scratch on next reconcile (the runtime cache is in-memory only). Cross-restart correctness for partially-finished workflows is deferred — Phase 2.6's CLI surface and Phase 4's TUI both want explicit retry/resume affordances anyway.

### 2.5 OpenSpec FS detection in the scheduler — ✅ shipped

Delivered:
- `openspec::affected_changes` — `snapshot(root)` walks `<root>/openspec/changes/*/` recursively and produces a `ChangesSnapshot { entries: BTreeMap<change_name, ChangeFingerprint { files: BTreeMap<rel_path, (size, content_hash)> }> }`. `diff(before, after)` returns a sorted, deduplicated list of change names whose fingerprints differ (added, removed, or any file changed). Content-hashed (`DefaultHasher` over file bytes) rather than mtime-based, so atomic-rename saves with identical bytes don't produce false positives.
- Orchestrator wiring — two new small maps, `prompt_cwds: HashMap<usize, PathBuf>` and `prompt_baselines: HashMap<usize, ChangesSnapshot>`. `PromptAdded`/`PromptUpdated`/`StateSnapshot` populate the cwd map (`worktree_path` wins over `cwd` because that's where edits actually land). `WorkerStarted` snapshots into `prompt_baselines`. `WorkerFinished` re-snapshots, diffs, and emits `ClientRequest::SetAnnotation { key: "openspec.affected_changes", value: [...] }` over the existing outbound channel. `PromptRemoved` cleans both maps.
- Empty diffs are written explicitly so consumers can distinguish *"watched, nothing changed"* from *"scheduler missed this prompt entirely"* (in which case no annotation appears at all). Phase 4's Drafts tab will rely on that distinction.
- The annotation flow runs for **every** prompt the scheduler observes, not just scheduler-dispatched ones — manual TUI prompts that touch `openspec/changes/` get auto-tagged the same way, which is the original Phase 0.3 user-visible feature, just hosted in the scheduler instead of the daemon.

21 new tests (workspace 569 → 590): 14 pure snapshot/diff cases (added/removed/modified files, nested specs/, dotfile filtering, scope limited to `openspec/changes/`) + 7 orchestrator scenarios driving synthetic daemon events through the outbound channel and asserting on the resulting `SetAnnotation` payloads (full diff, empty diff, missing baseline, missing cwd, worktree precedence, `PromptRemoved` cleanup, `StateSnapshot` cwd backfill).

### 2.6 CLI subcommands wired through — ✅ shipped

Delivered:
- New `commands.rs` module: every previously-stubbed subcommand (`queue`, `unqueue`, `drafts`, `status`, `templates path|edit`, `apply`, `archive`, `cancel`, `retry`, `propose`) is implemented as a pure function returning `Result<CommandOutput, CommandError>`. `main.rs` adapts the result to an `ExitCode` and writes stdout/stderr — the body is testable without spawning a process.
- FS-only flows touch `<root>/openspec/changes/` and `~/.local/share/clhorde/workflows/` directly:
  - `queue` writes the TOML marker; refuses if `openspec/changes/<name>/` doesn't exist.
  - `unqueue` removes the marker; missing marker is idempotent.
  - `drafts` filters `discovery::scan` for `Drafted` and prints sorted names.
  - `status` either lists every workflow as `<name>: <status> (<detail>)` or, when `<name>` is given, prints a labeled `key: value` block (priority, depends_on, queued_at, started_at, finished_at, prompt UUIDs). Missing workflow is an error.
  - `templates path` prints `~/.config/clhorde/scheduler/templates/`. `templates edit` creates the directory if missing then runs `$EDITOR` (with `$VISUAL` and `vi` fallbacks); failures propagate.
- Daemon-coupled flows go through a new `daemon_client::send_one_shot(requests)` helper that connects, sends every request, then sends a `Ping` and waits for `Pong` so we know the daemon dequeued every prior frame before we disconnect (otherwise the close races the daemon's read). Errors map to `OneShotError::{Unreachable, Disconnected, Timeout}` with the canonical "Is it running? Start with: clhorded" message on `Unreachable`.
- `apply <name>`: builds an in-process `Orchestrator` (no watcher), reconciles, calls `try_advance(name)`, drains the outbound channel, and ships every queued `SubmitPrompt` through the one-shot helper. Re-running picks up the next wave.
- `archive <name>`: renders the `archive` template and ships one `SubmitPrompt` with the canonical archive tag.
- `propose <idea>`: renders the `propose` template, ships one `SubmitPrompt` with `worktree=false` (the directory it'll create lives in the main repo, not a worktree).
- `cancel <name>`: removes the marker if present and updates the persisted workflow (`unqueue` if Queued, `cancel` if running). Daemon-side worker termination is intentionally not done here — a watching scheduler picks up the marker removal and cancels through the orchestrator. Standalone callers without a running scheduler get the persisted-state update for free.
- `retry <name> --section N`: resets `Failed` workflows back to `Implementing`, re-parses `tasks.md`, builds the DAG, finds node `N`, renders the apply template, and ships one `SubmitPrompt`.
- A scheduler-side control socket is still not in play — that's Phase 3.

23 new tests (workspace 590 → 613): 4 one-shot daemon helper cases (Ping/Pong, timeout, disconnect, irrelevant-event filtering) + 19 command tests covering every FS effect, error path (missing change dir, missing workflow, `templates edit` non-zero exit), and the request shape for daemon-coupled phase prompts.

## Phase 3 — `clhorde-cli flow` + scheduler control socket — ✅ shipped

Delivered:

- **Scheduler control socket** at `~/.local/share/clhorde/scheduler.sock` (helper `clhorde_core::ipc::scheduler_socket_path` lives next to the existing `daemon_socket_path`). Same length-delimited JSON framing as `clhorded`. New `clhorde_scheduler::control` module with three submodules:
  - `protocol` — `ControlRequest::{Ping, Status { name? }, Cancel { name }, Retry { name, section }}` and `ControlResponse::{Pong, Status { workflows }, Ok { message }, Error { message }}` plus `WorkflowSummary` (name, status label, optional `failure_reason`, priority, timestamps, prompt UUIDs). Every optional field is `#[serde(default, skip_serializing_if = "Option::is_none")]` for forward/back compat.
  - `server` — `spawn(Arc<Mutex<Orchestrator>>, socket_path)` runs the accept loop; per-client `run_with_streams` is generic over `AsyncRead + AsyncWrite` so unit tests drive both sides through `tokio::io::duplex`. Pure `dispatch_request(orch, req)` is exposed for in-process tests. Stale socket files are unlinked before bind.
  - `client` — `request(req)` and `request_at(path, req)` for one-shot calls; `request_many_at` keeps a connection open for sequenced requests. Friendly `ControlError::Unreachable` carries the canonical "Is it running? Start with: clhorde-scheduler daemon" message.
- **Orchestrator extensions** wired into the control surface without touching the FS/event handlers:
  - `summaries()` / `summary(name)` — pure read into the wire-format `WorkflowSummary`.
  - `cancel_workflow(name)` — best-effort marker removal then the existing `on_marker_removed` transition; returns `"unqueued"` / `"cancelled"` / `"noop"` so the caller can echo it.
  - `retry_section(name, section_id)` — resets `Failed → Implementing` if needed, re-parses `tasks.md`, rebuilds the DAG, renders the apply template, and ships one `SubmitPrompt` through the existing outbound channel. Refreshes the runtime cache so `note_prompt`/`WorkerFinished` correlate against the retry just like a freshly-dispatched node.
- **Daemon main-loop refactor**: the `Orchestrator` is now wrapped in `Arc<std::sync::Mutex<>>`. Each `select!` arm takes the lock briefly (no `.await` happens under the guard). The control socket spawn is non-fatal — if bind fails the daemon stays up, only remote control is degraded. SIGINT and orchestrator-channel-closed paths unlink the socket before exit.
- **`clhorde-cli flow <subcommand>`** in `commands/flow.rs` — `std::process::Command::new(bin).args(args).status()`. Binary resolution prefers the sibling-of-current-exe (`target/release/clhorde-scheduler` next to `clhorde-cli`) and falls back to `PATH`. Argument forwarding is verbatim; we don't re-derive a clap surface in `clhorde-cli`. Help text and the dispatch table are updated.

25 new tests (workspace 614 → 639):
- 1 ipc path-helper sanity check;
- 9 protocol JSON round-trips (incl. unknown-variant rejection and back-compat for `WorkflowSummary` defaults);
- 6 pure `dispatch_request` cases (Ping, empty/populated Status, named Status with unknown name, Cancel happy/unknown, Retry unknown);
- 3 duplex framing tests (`run_with_streams` Ping, malformed-JSON yields Error, two requests on one connection);
- 3 client tests against a real `UnixListener` (Ping round-trip, Unreachable on missing socket, request order preservation);
- 3 `clhorde-cli flow` wrapper tests (verbatim arg forwarding via a fake binary, exit-code propagation, missing-binary path).

## Phase 4 — TUI restructure

Sub-sliced like Phase 2:

- **4.1 — Foundation** (✅ shipped). Tab bar, `RootView { Prompts, Drafts, Workflows }`, scheduler-control client in the TUI, read-only Drafts/Workflows panes that poll the scheduler control socket every 2s while active. The Prompts tab is unchanged.
- **4.2 — Actions** (✅ shipped). `Q` queues the selected draft, `X` cancels the selected workflow, `T` opens an inline section prompt and dispatches a retry, `R` opens `proposal.md` (falling back to `design.md`) in `$PAGER`, `E` jumps back to Prompts in Insert mode with the scheduler root pre-filled as the cwd prefix.
- **4.3 — Detail view + freshness badges** (✅ shipped). Enter on a workflow zooms into a per-section DAG view with phase-by-phase dispatch state (running/completed/failed/pending + prompt id, exit code, deps). Esc closes; the same X/T/R action keys work in the overlay and target the open workflow. Title bars carry a "·  Ns" staleness suffix on the lists and the detail. Auto-refresh polls the open detail every 2s; push-based subscribe is deferred to Phase 5 because it needs a broadcast surface inside the scheduler that's larger than the rest of 4.3 combined.

### 4.3 Delivered

- **Protocol:** new `ControlRequest::Detail { name }` and `ControlResponse::Detail { detail: WorkflowDetail }`. `WorkflowDetail` carries the same top-level shape as `WorkflowSummary` plus three phase slots — `apply: Vec<DetailNode>`, `verify: Option<DetailNode>`, `archive: Option<DetailNode>`. Each `DetailNode` exposes `id`, `label`, `state` (`pending`/`running`/`completed`/`failed`), optional `prompt_id` / `prompt_uuid` / `exit_code`, and `depends_on` for apply nodes. All optionals serde-default for forward compat.
- **Scheduler:** `Orchestrator::detail(name)` merges the persisted `Workflow` with the in-memory `WorkflowRuntime` (DAG + per-node `NodeDispatch`). State labels come from a single `node_state_label` helper that mirrors the orchestrator's own state-machine, so the wire view never disagrees with reconcile decisions. `dispatch_request` plumbs the new variant; unknown workflows return `Error`.
- **TUI overlay:** new `App` fields `workflow_detail` / `detail_scroll` / `detail_last_poll`. Enter on Workflows queues a `Detail` request and synthesises an optimistic shell from the existing summary so the screen flips without a blank flash. Inside the overlay: j/k scroll, gg/G jump, r forces a refetch, Esc/Enter close, and Shift-X/T/R re-target the open workflow rather than the list selection. Action results that succeed force both `scheduler_last_poll` and `detail_last_poll` to `None` so the next 100ms tick re-renders fresh state.
- **Auto-refresh:** main.rs runs `dispatch_scheduler_action` for every drained pending request and additionally re-fetches the open detail every `DETAIL_REFRESH_INTERVAL` (2s). Detail responses route through `apply_workflow_detail`, which preserves the user's scroll position when only the same workflow is updated and resets it on workflow change. Detail errors close the overlay with a status-bar toast.
- **Freshness badges:** `App::scheduler_last_refresh_age_secs` / `detail_last_refresh_age_secs` drive a `freshness_suffix` formatter (`fresh` / `12s` / `3m` / `1h`) that the Drafts, Workflows and Detail title bars append. Returns `None` while the scheduler is unreachable so the unreachable banner remains the single source of truth.
- **Push-based subscribe deferred:** the original 4.3 also called for a long-lived `Subscribe` connection. That needs a broadcast channel inside the orchestrator and a stream-mode branch in the control server — a larger surface than the rest of 4.3. The 2s detail polling is good enough in practice; push moves to Phase 5.

22 new tests (workspace 682 → 704):
- 3 in `clhorde-core` (Detail request round-trip; Detail response round-trip; minimal `DetailNode` decode for back-compat).
- 5 in `clhorde-scheduler` (2 dispatch cases for Detail unknown/empty-apply; 3 orchestrator cases for `detail()` on Drafted, in-flight DAG with predecessors, and unknown workflow).
- 14 in `clhorde-tui`: 10 detail-overlay (Enter opens + queues Detail, Enter no-ops while unreachable, Esc closes without leaving tab, j/k scroll, r clears poll timer, Shift-X/T retarget the open workflow regardless of list selection, `apply_workflow_detail` keeps scroll on same-name updates / resets on rename, `detail_refresh_target` throttling, `note_detail_unreachable` closes + toasts) plus 4 freshness-badge cases (no-poll → `None`, unreachable → `None`, list age in seconds, detail age independent of reachability).

### 4.2 Delivered

- **Protocol:** `clhorde_core::control::ControlRequest::Queue { name, priority }` joins `Cancel` and `Retry`. `ControlResponse::Status` grew an optional `root: Option<String>` (back-compat: missing field deserializes to `None`) so the TUI can resolve `openspec/changes/<name>/...` and seed prompt cwds without a separate round-trip.
- **Scheduler:** `Orchestrator::queue_workflow(name, priority)` writes the marker (refusing if the change directory is missing) then re-uses `on_marker_created` so the state-machine path is identical to the watcher route. `dispatch_request` plumbs `Queue` to it and stamps `root` on every `Status` reply.
- **TUI:** `App` learned `scheduler_root`, `pending_scheduler_actions`, `retry_section_input`, `pending_pager_path` and a `take_pending_*` drain pair the main loop polls each iteration. `handle_root_view_key` now dispatches Shift-Q/X/T/E/R; the tab-switch digit shortcut is gated on `retry_section_input.is_none()` so dotted decimals starting with `1`/`2`/`3` aren't eaten.
- **Inline retry prompt:** Shift-T opens a centered popup capturing dotted decimals only (`[0-9.]+`); Enter dispatches `ControlRequest::Retry`, Esc closes it without firing. Empty submit warns via the status bar and closes the prompt so the user can re-open with one keystroke.
- **R / pager:** Shift-R picks `proposal.md` (fallback `design.md`); main.rs leaves the alt screen, runs `$PAGER` (default `less`), and restores. Missing files surface as a status message; pager binary failures don't crash the TUI.
- **E / explore:** Shift-E switches to Prompts + Insert mode and seeds the input with `<scheduler_root>: ` so the existing cwd-prefix parser routes the next prompt to the watched repo. Without a known root, the action no-ops with an explanatory toast.
- **Action results:** every dispatched request goes through the existing `sched_rx` channel as a new `ActionResult { ok, message }` outcome. `note_scheduler_action_result` flashes the message and clears `scheduler_last_poll` on success so the next 100ms tick refetches immediately.

24 new tests (workspace 652 → 682):
- 4 in `clhorde-core` (Queue round-trips with/without priority; `Status.root` round-trip + back-compat for the rootless legacy shape).
- 7 in `clhorde-scheduler` (3 `dispatch_request` cases for Queue happy/error and `root` propagation; 4 orchestrator cases for `queue_workflow` happy/empty-priority/missing-change/queue-then-cancel).
- 13 in `clhorde-tui` (Shift-Q/X happy paths and tab-mismatch noops; unreachable-scheduler guard; retry prompt collect/submit/cancel/empty + non-digit rejection; Shift-E with/without root; Shift-R picks proposal/design or warns; `apply_scheduler_status` keeps a known root when a later poll omits it; action-result toast forces a repoll on success).

### 4.1 Delivered

- **`clhorde_core::control` module** — moved the scheduler control protocol shapes (`ControlRequest`, `ControlResponse`, `WorkflowSummary`) here so the TUI doesn't have to depend on the scheduler crate (and pull in tera/notify/clap) just to decode wire types. The scheduler's `control::protocol` module re-exports for back-compat with the in-crate server/client. `chrono` in core gained the `serde` feature.
- **`scheduler_client`** in the TUI — one-shot async helper: connect to `~/.local/share/clhorde/scheduler.sock`, send one request, read one response, drop. 800ms timeout per call; failures map to `SchedulerError::Unreachable`/`Timeout`/etc. Mirrors the pattern of the existing `ipc_client.rs` rather than depending on the scheduler-side client.
- **`App` extension** — new fields `root_view`, `drafts`, `workflows`, per-tab selection indices, `scheduler_reachable`, `scheduler_last_poll`. New methods: `set_root_view`, `apply_scheduler_status` (one request feeds both tabs by splitting status `drafted` vs the rest), `note_scheduler_unreachable`, plus per-tab navigation helpers.
- **Key dispatch** — digit keys `1`/`2`/`3` switch tabs from Normal mode (only when no modifier is pressed; the digits remain available as text input in Insert/Filter/Interact). On Drafts/Workflows tabs, the prompt-list shortcuts are bypassed in favor of `j`/`k`/`g`/`G` navigation, `r` force-refresh, `Esc` to return to Prompts, `?` for help, `q` to quit (with worker confirmation).
- **UI rendering** — new top-of-screen tab bar (`[1] Prompts  [2] Drafts  [3] Workflows`). The main area is dispatched by `root_view`. Drafts/Workflows panes show empty-state and unreachable-scheduler hints. Workflow lines are color-coded by status (`drafted/queued/implementing/verifying/archiving/archived/cancelled/failed`). The input bar collapses to zero height on non-Prompts tabs. The bottom help bar swaps to a tab-specific hint set.
- **Polling** — main-loop `tick_interval` (100ms) checks `needs_scheduler_poll(&app)`. When the user is on a scheduler-backed tab and ≥2s elapsed since the last poll (or `scheduler_last_poll` is `None`, set on tab switch), a single `ControlRequest::Status { name: None }` fires fire-and-forget on a `tokio::spawn`; the result lands on a dedicated mpsc channel that the same `select!` consumes. Only one poll is in flight at a time.

13 new tests (workspace 639 → 652): 3 `scheduler_client` round-trips against a `tokio::net::UnixListener` (Pong, Status decode, Unreachable on missing socket), and 10 `App` tests (default `RootView`, digit-key switching, Esc returns to Prompts, `r` force-repolls, switching clears the poll timer, `apply_scheduler_status` splits + sorts + preserves selection by name across refreshes, navigation clamping, Prompts-tab shortcuts unaffected).

### Original sketch (reference)

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

## Phase 5 — Advanced

### 5.1 Push-based scheduler Subscribe — ✅ shipped

Delivered:

- **Wire protocol** (`clhorde_core::control`): new `ControlRequest::Subscribe` variant and a tagged `SchedulerEvent { Snapshot { workflows, root }, WorkflowUpdated { summary } }` carried as `ControlResponse::Event { event }`. `Snapshot.root` is `#[serde(default, skip_serializing_if = "Option::is_none")]` for forward compat with older scheduler builds. `SchedulerEvent` decodes strictly — unknown event kinds are an error so a future scheduler doesn't silently corrupt an old client's view.
- **Orchestrator emission**: `Orchestrator` now owns a `tokio::sync::broadcast::Sender<SchedulerEvent>` (capacity 256). Each public mutating method (`reconcile`, `cancel_workflow`, `queue_workflow`, `retry_section`, `handle_event`, `handle_daemon_event`, `try_advance`) is split into a public wrapper + a private `_inner` body. The wrapper snapshots `WorkflowSummary`s before, runs the body, then emits one `WorkflowUpdated` per workflow whose summary actually changed. Idempotent calls (no state change) emit nothing — important for keeping a Subscribe stream quiet when the user isn't doing anything. Internal call sites of `try_advance` switched to `try_advance_inner` to avoid double-emission. `Orchestrator::events_subscribe()` exposes a fresh `broadcast::Receiver` to the control server. `Orchestrator::new` signature is unchanged — the `Sender` is constructed internally.
- **Control server stream branch**: `run_with_streams` now branches on the first frame: if it's `Subscribe`, the connection switches to `run_subscribe_stream`. The handler subscribes to `events_subscribe()` *before* taking the snapshot (so events fired between snapshot and first `recv` aren't lost), writes one initial `SchedulerEvent::Snapshot`, then forwards every `WorkflowUpdated` over the same connection. `RecvError::Lagged` re-emits a fresh Snapshot so a slow subscriber converges back to a consistent baseline. Reads on the subscribe socket only detect EOF — extra frames sent by the client after Subscribe are silently dropped to preserve the one-way contract. `dispatch_request` rejects Subscribe with a clear error, since it should never reach the one-shot path in production. The non-Subscribe one-shot path was factored through a small `write_response` helper so both branches share one frame-writing path.
- **TUI subscribe client**: new `scheduler_client::subscribe()` returning `mpsc::UnboundedReceiver<SubscriptionMessage { Event, Disconnected }>`. The task connects, sends one `Subscribe` frame, then forwards every `Event` until the socket closes; emits one `Disconnected(SchedulerError)` frame on the way out so the owner can drive reconnect. The receiver hangup also tears the task down cleanly.
- **TUI main loop**: removed the periodic `ControlRequest::Status` poll and the `needs_scheduler_poll`/`SCHEDULER_POLL_INTERVAL` plumbing. The new `select!` arms are (a) `sched_event_rx.recv()` routing `Event` → `app.apply_scheduler_event` and `Disconnected` → `note_scheduler_unreachable + sched_subscription_alive = false`, and (b) `sched_reconnect_interval.tick()` re-spawning `subscribe()` when the subscription is down. Detail polling and Q/X/T action one-shots are unchanged.
- **`App::apply_scheduler_event`**: dispatches `Snapshot` through the existing `apply_scheduler_status` so the initial state lands the same way a poll did (selection preservation + freshness timer). `WorkflowUpdated` upserts a single summary into the right list — drafts vs workflows — by name. A workflow that transitions between `drafted` and any other status migrates between the two lists in one event without duplicating across them. Selection by name is preserved on each list across the upsert.

20 new tests (workspace 704 → 724):
- 5 in `clhorde-core` (Subscribe round-trip, Snapshot/WorkflowUpdated event round-trips, Snapshot back-compat without `root`, unknown-event-kind decode error).
- 6 in `clhorde-scheduler` orchestrator (Queue emits, Cancel emits, idempotent call emits nothing, reconcile emits one per discovered change, FS marker round-trip emits Drafted→Queued, multiple subscribers each receive every event).
- 3 in `clhorde-scheduler` control server (Subscribe end-to-end via `tokio::io::duplex` — initial Snapshot then WorkflowUpdated; multiple-client fan-out; extra frames after Subscribe ignored).
- 2 in `clhorde-tui` `scheduler_client` (subscribe forwards events until disconnect against a real `UnixListener`; missing socket reports `Disconnected(Unreachable)` and closes the channel).
- 6 in `clhorde-tui` `App` (Snapshot populates lists; WorkflowUpdated inserts new draft / new active; drafted→queued migration removes from drafts and inserts into workflows; in-place replacement keeps a single entry; insertions stay alphabetically sorted).

The detail overlay still uses 2s polling — a Detail subscription needs a per-workflow event surface, broken out into Phase 5.3.

### 5.2 Web routes + dashboard tabs — ✅ shipped

Delivered:

- **`clhorde-web::scheduler_bridge`** — long-lived bridge to the scheduler control socket. Mirrors `DaemonBridge`'s shape but adapts to the lighter wire protocol: a single Subscribe connection feeds a `tokio::sync::broadcast::Sender<SchedulerEvent>`, while one-shot `request()` opens a fresh connection per RPC for status/detail/queue/cancel/retry. Critical property: **`SchedulerBridge::start` does not block startup** — `clhorde-web` happily serves the daemon-only views (Prompts/Store) when `clhorde-scheduler daemon` isn't running, and reconnects in the background with capped exponential backoff (250ms → 15s). `request(Subscribe)` is rejected with a clear error to avoid deadlocking callers.
- **REST surface** (5 routes, all under `/api/scheduler/`):
  - `GET status` → `{ workflows, root, connected }`. Same payload as the TUI's `ControlRequest::Status { name: None }` plus a `connected` flag the SPA uses for the unreachable banner.
  - `GET workflow/{name}` → `WorkflowDetail` (DAG nodes + per-phase dispatch state). 404 when the workflow is unknown — distinct from 503 ("scheduler not running").
  - `POST workflow/{name}/queue` (body: `{ priority? }`) → writes the `.clhorde-ready` marker.
  - `POST workflow/{name}/cancel` → removes marker + transitions.
  - `POST workflow/{name}/retry` (body: `{ section }`) → re-dispatches one apply node.
  - Error mapping: `Unreachable` → 503, `Timeout` → 504, `Io`/`BadResponse` → 502, `ControlResponse::Error` → 400 (mutations) / 404 (Detail).
- **WebSocket fan-out** — `handle_ws` now subscribes to `scheduler.subscribe_events()` alongside the daemon broadcasts. Every WS client receives `{type: "SchedulerEvent", event: <SchedulerEvent>}` frames as the scheduler emits them. On connect, if the scheduler bridge is `is_connected()`, the handler issues a one-shot `Status` and forwards it as a Snapshot frame so the SPA bootstraps without a separate REST call. Lag handling: scheduler-side already re-emits Snapshots on `RecvError::Lagged`, so the WS arm just logs and skips.
- **Auth + CORS unchanged** — both layers wrap the new routes via the same middleware as the daemon routes; the auth token also gates the WS upgrade.
- **SPA frontend** (`static/index.html`, `app.js`, `style.css`):
  - Two new top-nav tabs: **Drafts** + **Workflows**. The existing Dashboard/Store tabs are untouched.
  - `AppState` learned `drafts: string[]`, `workflows: WorkflowSummary[]`, `schedulerRoot`, and `schedulerSeen` (true once any SchedulerEvent arrived). `update()` dispatches `SchedulerEvent` envelopes, and `_applySchedulerEvent` mirrors the TUI's `apply_scheduler_event`: Snapshot rebuilds both lists from scratch, WorkflowUpdated upserts by name and migrates entries between Drafts and Workflows when `status` crosses the `drafted` boundary. `appState.onChange` triggers a re-render of whichever scheduler tab is currently open, so push events land without a manual refresh.
  - Drafts pane: list of names with a `[Queue]` button per row. Empty state distinguishes "no drafts yet" from "scheduler not reachable" using `schedulerSeen`.
  - Workflows pane: status-coloured rows (`badge-workflow-{drafted,queued,implementing,verifying,archiving,archived,failed,cancelled}`). `[Open]` zooms into a detail block fetched on demand via `GET workflow/{name}`; `[Cancel]` runs the REST cancel with a `confirm()` gate. Detail block renders apply DAG nodes (id, label, state badge, prompt id, exit code, deps), then verify/archive singletons. Inline section-id input + `[Retry section]` button targets the open workflow.
  - CSS: scheduler-views reuse the `store-view` layout (full-width, scroll, padded). New badge classes for workflow + node states matching the existing pending/running/completed/failed palette. Workflow nodes use a left border colour-coded by state.

14 new tests (workspace 724 → 738):
- 5 in `clhorde-web::scheduler_bridge` (start does not block when scheduler offline; subscribe loop forwards events to subscribers; request returns Unreachable when scheduler offline; one-shot request round-trip; Subscribe rejected on `request()`).
- 7 in `clhorde-web::routes` (Queue body defaults + with priority; Retry body requires section + rejects empty; error mapping for Unreachable→503, Timeout→504, Io→502).
- 2 in `clhorde-web::ws` (`scheduler_message` envelope wraps Snapshot + WorkflowUpdated with the right `type` discriminants).

The web dashboard keeps the existing 1495-line vanilla-JS SPA approach — no build step, no framework. The new tabs add ~290 lines of JS and ~90 lines of CSS.

### 5.3 Detail subscription — ✅ shipped

Goal: kill the 2s polling that powers the workflow detail overlay (TUI) and the expanded workflow card (web SPA). Replace it with push-based detail events delivered over the same Subscribe-style stream Phase 5.1 introduced for summaries. Mirrors 5.1 but scoped to one workflow at a time, since `WorkflowDetail` is heavier than `WorkflowSummary` and most viewers only care about one workflow.

Sliced into three independently-shippable sub-phases:

#### 5.3.1 Wire protocol + orchestrator + control server

- **Wire protocol** (`clhorde_core::control`):
  - New `ControlRequest::SubscribeDetail { name }` — switches the connection into a per-workflow stream. Symmetric with `Subscribe` (one-way after the request, separate connection for follow-up RPCs).
  - Two new `SchedulerEvent` variants: `DetailSnapshot { detail }` (initial frame on a SubscribeDetail connection, also re-sent on broadcast lag) and `DetailUpdated { detail }` (per orchestrator state change). Both carry a full `WorkflowDetail` rather than a diff — payloads are small (≤ a few dozen nodes) and full-snapshot semantics keep the client logic trivial.
  - `SubscribeDetail` against an unknown workflow returns `ControlResponse::Error { message }` and closes the connection (no frame stream, no event ever fired).
- **Orchestrator emission**: add a second `broadcast::Sender<SchedulerEvent>` dedicated to detail events (capacity 256). The mutating-method wrapper pattern from 5.1 grows a `snapshot_details()` / `emit_detail_diff()` pair that mirrors the existing summary diff. We always compute details before+after the inner body and emit one `DetailUpdated` per workflow whose `WorkflowDetail` actually changed — this catches per-node state shifts (e.g. `WorkerFinished` for one apply node) that don't move the surrounding `WorkflowSummary`. A new `Orchestrator::detail_events_subscribe()` exposes a fresh `broadcast::Receiver`.
- **Control server**: new `run_subscribe_detail_stream` branch in `run_with_streams`. Same shape as `run_subscribe_stream` but: (a) takes `name` from the request, (b) on missing workflow writes one `Error` response and returns, (c) filters every event from the broadcast by `detail.name == name` so a per-workflow subscriber only ever sees its own workflow.

#### 5.3.2 TUI integration — drop polling — ✅ shipped

Delivered:

- **`scheduler_client::subscribe_detail(name)`** mirrors `subscribe()`. Reuses the same `SubscriptionMessage` enum (Event carries `SchedulerEvent::DetailSnapshot`/`DetailUpdated`; Disconnected carries the underlying `SchedulerError`). The shared `drive_subscribe` helper now takes the request as a parameter and adds an explicit branch for `ControlResponse::Error` → `Disconnected(BadResponse(message))` so a SubscribeDetail against an unknown workflow surfaces the server's error verbatim instead of a generic "unexpected non-event frame". 2 new tests in `scheduler_client.rs` (assert the wire frame the client sends + assert the BadResponse path).
- **Main loop**: new `DetailSubscription { target, rx, snapshot_seen }` held on the run-loop task. Each tick reconciles `app.detail_subscription_target()` against `detail_sub.target` — opening / closing / switching the overlay drops or spawns the underlying SubscribeDetail connection automatically. The dedicated select arm uses `std::future::pending()` as the no-overlay sentinel so it contributes no wakeups when nothing is open. On Disconnected: if `snapshot_seen=false`, surface `note_detail_unreachable` (the workflow likely doesn't exist or the scheduler is down); if true, drop the receiver but keep the overlay — the next reconcile pass re-spawns the subscription.
- **Polling removed**: the `DETAIL_REFRESH_INTERVAL` branch, the optimistic `app.detail_last_poll = Some(Instant::now())` stamp before each one-shot, and `App::detail_refresh_target` are gone. `App::detail_last_poll` itself stays — repurposed to track "last DetailSnapshot/DetailUpdated received Ns ago" for the freshness badge in the overlay title (the `apply_workflow_detail` stamper already does the right thing under push semantics).
- **Forced refresh**: the `r` keybinding inside the detail overlay no longer clears the poll timer — it now queues an explicit `ControlRequest::Detail` through `pending_scheduler_actions`, which `main.rs` dispatches as a one-shot. Push events keep the overlay live on their own; `r` is the escape hatch for a user who wants to confirm liveness manually.
- **Action-result fix-up dropped**: `App::note_scheduler_action_result` no longer clears `detail_last_poll` on success, since the orchestrator's `emit_diff` pushes `DetailUpdated` within ms of the mutation committing.
- **`App::apply_detail_event`**: dispatches `DetailSnapshot`/`DetailUpdated` through the existing `apply_workflow_detail` and drops events whose `detail.name` doesn't match the open overlay (race during overlay-switch when the prior subscription is still tearing down on the orchestrator side). Summary-stream variants (`Snapshot`/`WorkflowUpdated`) reach the no-op arm — they shouldn't arrive here in production but the App stays robust under a forward-compat scheduler that multiplexes.

5 net new tests in `clhorde-tui` (workspace 750 → 755):
- 2 in `scheduler_client` (subscribe_detail wire round-trip; unknown workflow → `BadResponse`).
- 4 in `app` (`detail_subscription_target` reflects overlay state; `apply_detail_event` routes Snapshot+Update + drops mismatched-name frames; `apply_detail_event` ignores summary variants; `note_scheduler_action_result` no longer touches `detail_last_poll`).
- −1 obsolete test (`detail_refresh_target_throttled` — function removed).
- 1 test rewritten (`r_inside_detail_clears_poll_timer` → `r_inside_detail_queues_explicit_detail_request` since `r`'s semantics changed).

#### 5.3.3 Web bridge + SPA push — ✅ shipped

Delivered:

- **New wire variant `ControlRequest::SubscribeAllDetails`**: an unfiltered detail stream the bridge consumes. The orchestrator's `detail_events` broadcast already emits every workflow's `DetailUpdated`; the new server handler `run_subscribe_all_details_stream` forwards them without per-name filtering and without an initial snapshot (clients REST-fetch the seed for whatever card they expand). `dispatch_request` rejects the variant on a one-shot connection. 1 new round-trip test in `clhorde-core` + 1 server-side stream test (two workflows mutate, both `DetailUpdated` frames surface).
- **`SchedulerBridge` parallel detail loop**: a second long-lived task `subscribe_all_details_loop` opens a `SubscribeAllDetails` connection and forwards every event into the same `event_tx` broadcast the summary loop writes to. Reconnects independently with the same capped exponential backoff (250 ms → 15 s) so a transient blip on one stream doesn't drop the other. The `connected` flag still tracks summary connectivity only — that's the signal the SPA banner reads. The bridge's `request()` now rejects `Subscribe`, `SubscribeAllDetails`, and per-workflow `SubscribeDetail` with a clear `BadResponse` so callers can't accidentally one-shot a stream-mode request and deadlock. 5 new tests in `clhorde-web::scheduler_bridge` (details forwarding to subscribers; combined summary+detail multiplexing onto one broadcast; rejection of all three subscribe variants on `request()`).
- **WS envelope**: `scheduler_message` already wraps any `SchedulerEvent` through serde, so `DetailSnapshot` and `DetailUpdated` ride the existing `{type:"SchedulerEvent",event:…}` envelope without changes. 2 new envelope tests (`detail_snapshot` and `detail_updated` produce the expected `event.type` discriminator).
- **SPA push detail**: `_applySchedulerEvent` learned `detail_snapshot` / `detail_updated` cases — when the event's `detail.name` matches `expandedWorkflow`, it replaces `expandedWorkflowDetail` and calls `renderWorkflowDetail()`. Off-screen workflows are silently dropped. Crucially: `cancelWorkflow` and `retrySection` no longer issue a follow-up `fetchWorkflowDetail` after the action — the orchestrator's `emit_diff` pushes `DetailUpdated` within ms of the mutation committing and the SPA re-renders from the push stream. The initial render of an expanded card still uses `GET /api/scheduler/workflow/{name}` for an immediate paint.

8 new tests (workspace 755 → 763): 1 in `clhorde-core` (`SubscribeAllDetails` round-trip), 1 in `clhorde-scheduler::control::server` (unfiltered stream forwards every workflow), 5 in `clhorde-web::scheduler_bridge` (3 reject-on-request + 2 forward), 2 in `clhorde-web::ws` (envelope shape).

Open questions (to resolve during 5.3.1):
- Does the orchestrator need a `WorkflowRemoved` event too, paired with detail teardown? Decision deferred — workflows don't currently disappear from `workflows`, same as 5.1.
- Should SubscribeDetail accept multiple names? Decision: no, one workflow per connection. Two viewers = two connections; the orchestrator broadcast fans them out.

### 5.4 Inter-workflow dependencies — ⏳ in progress

Goal: honor `depends_on: ["other-change-name", …]` in `.clhorde-ready` so a workflow stays held until every named dependency reaches `Archived`. `MarkerMetadata.depends_on` is already parsed (Phase 0.x) and surfaced read-only by `flow status`, but no gate enforces it — the orchestrator currently transitions any `Queued` workflow into `Implementing` as soon as a worker slot opens up. This phase wires the gate, surfaces *why* a workflow is held, and propagates dep failures so dependents don't dangle silently.

Design choices:

- **No new `Blocked` status.** A "soft hold" is just `Queued` with a non-empty `blocked_by`. Avoids a state-machine variant + persistence migration + new badge palette for what is fundamentally a runtime pause. Forward-compat for older clients is automatic since the new field defaults to empty.
- **Gate sits in `advance_queued`** (orchestrator.rs). Before `start_implementing()`, evaluate `wf.metadata.depends_on` against the current `workflows` map. If any dep is missing-from-disk, `Cancelled`, `Failed`, or part of a cycle, fail the dependent with a clear reason. If any dep is `Drafted`/`Queued`/`Implementing`/`Verifying`/`Archiving`, return `Ok(())` and stay queued. Only an all-`Archived` dep set unblocks the transition.
- **Cancelled / Failed deps propagate.** A dependent whose dep ends up `Cancelled` or `Failed` is itself failed with `dependency '<name>' <state>`. Rationale: the dep produced no artifact, so silently stalling the dependent forever is worse than telling the user explicitly. Retry path: user removes the dep marker, re-drops one, then `flow retry` the dependent (or re-queues it).
- **Cycle detection** at `advance_queued` time. Walk `depends_on` transitively across the orchestrator's view; if a cycle including this workflow is reachable, fail every member with `dependency cycle: A → B → A`. Cheap (workflows are O(dozens), not O(thousands)) and deterministic.
- **Cascading wake.** When a workflow transitions to `Archived`, sweep `workflows` and `try_advance_inner` every entry that lists it in `depends_on`. Same hook re-runs cycle detection, so a previously-stuck dependent that is now satisfiable picks up automatically without an explicit FS event.
- **Surface in `WorkflowSummary` + `WorkflowDetail`** via a new `blocked_by: Vec<String>` field (defaults to empty, `#[serde(default, skip_serializing_if = "Vec::is_empty")]`). Always populated for `Queued` workflows whose deps aren't all archived, empty otherwise. The existing `emit_diff` helpers fire `WorkflowUpdated` / `DetailUpdated` automatically when this field shrinks (i.e. when a dep clears), so TUI/web repaint without new event plumbing.
- **No marker file format change.** `depends_on = ["a", "b"]` already round-trips through TOML.

Sliced into three independently-shippable sub-phases:

#### 5.4.1 Core gating + cycle detection + dep-failure propagation

- New `orchestrator::deps::evaluate(name, workflows)` returning a `DepEvaluation` enum: `Satisfied`, `Pending(Vec<String>)`, `Failed(String)`. Pure function over a snapshot — easy to unit-test exhaustively without spinning up a full orchestrator.
- `advance_queued` consults `evaluate` before `start_implementing`. `Pending` keeps the workflow in `Queued`, `Failed` calls `wf.fail(reason)`, `Satisfied` proceeds as today.
- Cycle detection: depth-first walk over `depends_on` edges restricted to known workflow names. If `name` revisits itself, every workflow in the cycle is failed with the same `dependency cycle: A → B → A` reason.
- `advance_archiving` post-archive: collect dependents of `wf.name` from `workflows` and call `try_advance_inner` on each. Bounded by orchestrator size; recursion impossible because an archived workflow can't re-trigger the cycle.
- Tests: ~12 in `orchestrator::deps` (satisfied/pending/missing-dep-fails/cancelled-dep-fails/failed-dep-fails/two-step chain holds then unblocks/cycle-of-2/cycle-of-3/self-dep/transitive satisfied/archive cascade re-evaluates dependents/idempotent on already-failed dependent).

#### 5.4.2 `blocked_by` on Summary + Detail + push events — ✅ shipped

Delivered:

- **`blocked_by: Vec<String>`** added to both `WorkflowSummary` and `WorkflowDetail` (`clhorde-core::control`). `#[serde(default, skip_serializing_if = "Vec::is_empty")]` keeps the wire payload byte-identical to the pre-5.4 shape when no deps are blocking — older clients see no new field, older schedulers' payloads decode cleanly with an empty default. Both directions of the version skew are covered.
- **`workflow_summary` helper** grew a `&BTreeMap<String, Workflow>` parameter and re-runs `deps::evaluate` for `Queued` workflows. `Pending` → those names land in `blocked_by`; `Satisfied` / `Failed` / non-`Queued` → empty. This means even a workflow that just transitioned from `Drafted` to `Queued` immediately surfaces its blockers without waiting for the next try-advance pass. All call sites (`summaries`, `summary`, `detail`, `snapshot_state`, `emit_diff`) now thread `&self.workflows`.
- **`detail()`** mirrors the field by reading the value the helper computed (no second evaluator call).
- **Diff machinery** transparently picks up the new field — `WorkflowSummary` / `WorkflowDetail` already drive their respective `WorkflowUpdated` / `DetailUpdated` events through `PartialEq`. A dep clearing → `blocked_by` shrinks → exactly one push event fires. No new event variants, no new diff plumbing.
- **Test sites** (`clhorde-tui::app`, `clhorde-tui::scheduler_client`, `clhorde-web::ws`, `clhorde-web::scheduler_bridge`) updated to construct `WorkflowSummary` / `WorkflowDetail` literals with `blocked_by: vec![]` — those are pure test fixtures, not production code paths.

6 new tests (workspace 785 → 791):
- 3 in `clhorde-core::control` (round-trip with non-empty `blocked_by`; `skip_serializing_if` omits the field when empty so the wire stays back-compat with old schedulers; `WorkflowDetail` round-trip + decoding a legacy payload without the field).
- 3 in `clhorde-scheduler::orchestrator` (`summary().blocked_by` populated for pending Queued; empty for Drafted + Archived; `WorkflowUpdated` carrying empty `blocked_by` after the dep cascade-archives — final value of x reflects the cleared list).

#### 5.4.3 CLI + TUI + Web surfacing

- `clhorde-cli flow status <name>`: enrich the existing `depends_on:` line to `depends_on: A (archived), B (queued — blocking)`. When `blocked_by` is non-empty, also print `blocked_by: A, B` so it's grep-friendly.
- TUI Workflows pane: append `· blocked` to the status badge when `blocked_by` is non-empty (e.g. `queued · blocked`). Detail overlay header gains a `Blocked by: A, B` line above the apply DAG when populated.
- Web SPA: matching badge suffix in the workflow row + `Blocked by: A, B` line in the expanded card. CSS reuses the existing dim-orange accent class.
- 4–6 small tests across the three layers (CLI snapshot of `flow status` text; TUI render path for the badge suffix; SPA: 1 unit test that the badge appears when `blocked_by` is non-empty).

Open questions (to resolve during 5.4.1):

- **Should a `Drafted` dep block forever, or be treated as a bad-dep?** Recommend block forever — user might intend to queue it later. Make the "stuck" state visible enough via `blocked_by` that users notice. (Counter-arg: typo workflow name silently stalls everything that depends on it. Mitigation: if the name doesn't exist *as a workflow at all* — i.e. no `openspec/changes/<name>/` directory — fail immediately with `dependency '<name>' not found`. Drafted deps still hold.)
- **Manual override knob?** A `--ignore-deps` flag on `flow queue` that strips `depends_on` from the marker before write. Defer to 5.x unless needed.
- **Do we need `WorkflowFailed` as a distinct event for the cascade?** No — `WorkflowUpdated` already carries the new `Failed` status and the existing diff machinery emits it. Dependents-of-failed-dep get their own `WorkflowUpdated` as the cascade fails them.

### 5.x Remaining (deferred)

- **Disjoint-files analysis**: refuse `parallel-with` annotations when sections touch overlapping files (cheap heuristic via grep over `proposal.md` / `design.md`).
- **Auto-retry policies** per phase (apply: 2, verify: 1, archive: 0; configurable).
- **Hooks**: `post-archive: gh pr create --title "{{change_name}}"`.
- **Multi-repo**: a scheduler instance can watch several repos.
- **`--ignore-deps` flag** on `flow queue` (manual override for the gate added in Phase 5.4).

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
| **2.1** | ✅ `9382e38` | Binary skeleton + clap CLI + long-lived daemon client | Foundation; all later sub-phases bolt onto this. |
| **2.2** | ✅ shipped | Discovery + workflow types + persistence | Pure data layer; 32 new tests, no watcher needed. |
| **2.3** | ✅ shipped | FS watcher + state machine wiring | Reactivity without prompt dispatch yet; 26 new tests + live smoke. |
| **2.4** | ✅ shipped | Tera templates + prompt dispatch via daemon | First end-to-end run of a real workflow; 35 new tests. |
| **2.5** | ✅ shipped | `openspec/changes/` snapshot in scheduler → `SetAnnotation` writes | User-visible auto-link, agnostic-daemon-friendly; 21 new tests. |
| **2.6** | ✅ shipped | One-shot CLI subcommands implemented | Scriptable usage end-to-end; 23 new tests. |
| **3**   | ✅ shipped | `clhorde-cli flow` wrappers + scheduler control socket | Single CLI entrypoint; live remote-control of a long-lived scheduler; 25 new tests. |
| **4.1** | ✅ shipped | TUI tabs foundation (read-only Drafts/Workflows) | First-class UX, no actions yet; 13 new tests. |
| **4.2** | ✅ shipped | Tab actions: Q/E/R/X/T (+ scheduler `Queue` + `Status.root`) | Queue, explore, review, cancel, retry — 24 new tests. |
| **4.3** | ✅ shipped | Workflow detail view + freshness badges (push subscribe deferred) | Per-section DAG zoom + 2s auto-refresh — 18 new tests. |
| **5.1** | ✅ shipped | Push-based scheduler `Subscribe` + broadcast-backed event stream | Replaces 2s status polling — 20 new tests. |
| **5.2** | ✅ shipped | Web routes + Drafts/Workflows SPA tabs | Browser dashboard reaches feature parity with TUI tabs — 14 new tests. |
| **5.3.1** | ✅ shipped | `SubscribeDetail` wire + orchestrator detail broadcast + control-server stream branch | Server-side push surface for one workflow's `WorkflowDetail` — 12 new tests. |
| **5.3.2** | ✅ shipped | TUI: spawn/abort detail subscription with overlay; drop 2s polling | Removes the last polling path inside the TUI — 5 net new tests. |
| **5.3.3** | ✅ shipped | Web: `SubscribeAllDetails` + bridge fan-in + SPA push handler | SPA reaches push parity with the TUI — 8 new tests. |
| **5.4.1** | ✅ shipped | Inter-workflow deps: gate in `advance_queued` + cycle detection + dep-failure propagation | Pure orchestrator logic — 22 new tests. |
| **5.4.2** | ✅ shipped | `blocked_by: Vec<String>` on Summary + Detail; populate via evaluator; back-compat round-trip | Wire surface so push events carry the "why blocked" — 6 new tests. |
| **5.4.3** | ⏳ pending | CLI `flow status` enriched + TUI badge suffix + Web badge + Blocked-by line | Presentation layer — no logic, just rendering. |
| **5.x** | ⏳ pending | Advanced (parallel safety, hooks, multi-repo, manual `--ignore-deps`) | Polish. |

Each phase is independently shippable. Phase 0 alone is already a feature win; Phase 2.4 already produces value for users who want a scriptable workflow runner.

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
