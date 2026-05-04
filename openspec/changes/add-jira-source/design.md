## Context

The scheduler crate (`clhorde-scheduler`) already anticipates multiple workflow sources — `openspec/mod.rs` literally calls out "future workflow sources (Linear, GitHub Issues, custom YAML)". The current OpenSpec source has three pieces in a clean shape:

1. **`watcher.rs`** turns notify events into a small `FsEvent` enum (`MarkerCreated`, `MarkerRemoved`, `TasksModified`).
2. **`discovery.rs`** does an initial reconciliation scan and parses `.clhorde-ready` TOML markers into `MarkerMetadata`.
3. **The orchestrator** consumes `FsEvent`s via `handle_event()` and drives a fixed FSM (`Drafted → Queued → Implementing → Verifying → Archiving → Archived`) with the usual `Cancelled` / `Failed { reason }` terminal escapes.

The natural seam for a new source is "produce events the orchestrator can consume" — the runtime detail of push (notify) vs pull (poll) is invisible past the channel. The harder questions are around lifecycle semantics, since Jira-driven workflows need a "human is reviewing" beat that the OpenSpec FSM doesn't currently model.

The shape we converged on during exploration:

> The scheduler sees a Jira ticket. It runs `/opsx:explore <ticket content>` in an interactive PTY worker. The workflow parks until a human writes `.clhorde-ready` (via `clhorde-cli approve`). Once the marker lands, the existing OpenSpec watcher fires and the rest of the workflow runs unchanged.

This makes the Jira source own only the front of the lifecycle. The OpenSpec source — which already knows how to take a complete change directory through Implementing → Archived — is reused as-is. The two sources cooperate on one workflow.

## Goals / Non-Goals

**Goals:**
- Ingest Jira issues matching a JQL filter into scheduler workflows, keyed by issue key.
- Run `/opsx:explore` against each ticket, in an interactive worker that a human can attach to.
- Park the workflow in an `Exploring` state until either `.clhorde-ready` (approve) or `clhorde-cli reject` is observed.
- Make the existing OpenSpec source unaware that Jira exists — it just sees a complete change directory and a marker, like always.
- Surface enough state in CLI/TUI/Web (`status`, `list`, the `blocked_by` column already shipped in 5.4.x) for users to see Jira-triggered workflows.
- Write back to Jira on lifecycle events (comment on spawn, comment on archive, comment on reject; status transitions optional).

**Non-Goals:**
- Subtask DAGs, AC checklist parsing, structured ticket decomposition.
- A "review proposals" UI inside TUI/web. Humans review by reading files in their editor.
- Direct mode (`mode = "direct"`, single-prompt no-OpenSpec) is wired through config but the implementation is deferred to a follow-up change. This change ships only `mode = "openspec"`.
- Multi-instance support. One Atlassian site per scheduler.
- Auto-approval of proposals. There is no `clhorde-yolo` mode.
- Refactoring the entire `WorkflowStatus` FSM into a generic shape. We extend it conservatively for now; a future change can generalise if a third source forces it.

## Decisions

### D1 — One workflow per ticket, name = issue key

Workflows are keyed by `name: String` today. The Jira source uses the issue key (`PROJ-1234`) directly. This means:

- Re-creating an existing workflow is a no-op — solves the polling de-duplication problem with zero state.
- The change directory under `openspec/changes/PROJ-1234/` lines up 1:1 with the workflow, so the existing OpenSpec watcher fires correctly.
- Slugs (`PROJ-1234-add-oauth`) are deferred. They're nicer to read but expensive in plumbing (need a key↔slug mapping somewhere durable). A future change can add them.

**Alternatives considered:**
- Random UUIDs with a `jira_key` field — cleaner abstractly but breaks the "drop a change dir, watcher picks it up" symmetry.
- Slug from ticket title — readable, but ticket titles change and slugs would need to be stable.

### D2 — Two-source cooperation, not a generic source trait

The Jira source produces events that drive the workflow into and through `Exploring`. Once the human writes the marker, the OpenSpec watcher (already running) fires `MarkerCreated`, which is the existing entry point to `Queued`. **The orchestrator doesn't need to know which source created the workflow** to run the back half — it just runs the same code it always did.

We do *not* introduce a `Source` trait or a generic source registry in this change. Two concrete sources, hand-wired in the orchestrator's startup, is enough for now. The trait abstraction can wait until a third source forces it.

**Why:** YAGNI. Building a trait for two implementations where one strongly anticipates the other's data shape (a marker file on disk) is premature.

### D3 — `Exploring` is one new state, with three exits

Conservative FSM extension:

```
Drafted ─────────────────▶ Queued ─▶ Implementing ─▶ Verifying ─▶ Archiving ─▶ Archived
                              ▲
        ┌─────────────────────┘
        │ MarkerCreated (from existing watcher)
        │
Triggered ─▶ Exploring ──┤
   (Jira)        │       │
                 │       └─ ExploreFailed → Failed { reason }
                 │
                 └─ RejectRequested → Cancelled
```

`Exploring` is entered from `Triggered` (a transient state immediately after Jira creates the workflow, used only to dispatch the explore worker). It is exited via:

1. **`MarkerCreated` for this workflow's name** — the human approved. Transition to `Queued`. If the explore worker is still alive, kill it gracefully (SIGTERM) before the transition completes.
2. **`RejectRequested` (CLI signal)** — transition to `Cancelled`. Kill the worker, delete the change directory if it exists, write back to Jira.
3. **Worker exits with a non-zero status while still in `Exploring` and no marker yet** — log, but stay in `Exploring`. The artifacts (if any) are still on disk; the human can still approve, reject, or spawn a new explore session.

We do **not** add a separate "ProposalReady" state to distinguish "worker live" from "worker exited but waiting." The distinction is observable from the prompt's status (running vs. completed) and surfacing it in the FSM was discussed and rejected as cosmetic.

**Alternatives considered:**
- Generalise the FSM to `Queued → Running → Done` and demote OpenSpec phases to sub-states. Right long-term, but a much larger refactor for one new source.
- Reuse `Drafted` for the parked-waiting-for-human state. Rejected — `Drafted` means "no work has happened yet" and we'd lose that signal.

### D4 — Polling, with `If-Modified-Since`-style cursor

The Jira source runs an `async` poll loop with configurable cadence (default 30s, min 15s). Each poll:

1. Issues a JQL search against the configured filter (one filter per "queue").
2. Diffs the result against last-known state, emitting `JiraEvent::TicketAppeared { key, payload }` for new keys and `JiraEvent::TicketLeftFilter { key }` for keys that were present before but aren't anymore.
3. The orchestrator consumes those events through the same `handle_event` style as `FsEvent`.

State for the diff is held in memory; the durable state lives in the workflow store (workflows themselves remember their Jira origin via a new `source: SourceKind` field). On scheduler restart, the first poll re-emits `TicketAppeared` for everything still in the filter — but those calls are no-ops because the workflows already exist.

**Alternatives considered:**
- Webhooks. Lower latency, no polling — but require Jira admin to configure a webhook target, public ingress, signature verification. Overkill for v1; can be added later as a parallel transport feeding the same event channel.
- Per-poll rate-limit backoff on 429s with jitter. Will use `governor` or hand-rolled exponential backoff.

### D5 — Worker is interactive (PTY), seeded with a templated `/opsx:explore`

The explore worker is dispatched via the existing PTY worker path (`Prompt::interactive`). The prompt template:

```
/opsx:explore

You've been auto-spawned by the clhorde scheduler from a Jira ticket.
No human is here yet — they will attach via TUI or web shortly.

When a human arrives:
- Greet them, summarise what you understood from the ticket
- Ask the clarifying questions that emerge naturally
- When they signal you have enough to draft the change, create the
  proposal under `openspec/changes/<KEY>/`

The change directory MUST be named exactly `<KEY>` so the scheduler can
match it to the Jira ticket.

Until the human attaches, output a brief opening response acknowledging
the ticket and listing the clarifying questions you'd want answered. Do
NOT write any artifacts yet.

--- TICKET <KEY> ---
Title: <TITLE>
Description: <DESCRIPTION>
Acceptance Criteria: <AC>
Labels: <LABELS>
Reporter: <REPORTER>
```

The template lives in the source, not a config file, in this first cut. A `keymap.toml` override hook can come later if anyone wants it.

### D6 — Approve and reject are explicit CLI subcommands

`clhorde-cli approve <id>` writes `openspec/changes/<id>/.clhorde-ready` and gracefully kills any live explore worker for that workflow (the existing OpenSpec watcher fires on the new file and drives the rest).

`clhorde-cli reject <id>` removes the change directory entirely, kills the worker, transitions the workflow to `Cancelled`, and writes back to Jira (comment + remove the trigger label so the next poll doesn't re-create it).

Both commands route through the daemon over IPC — this matches the rest of `clhorde-cli`'s shape and avoids racy direct file manipulation.

**Alternatives considered:**
- A `.clhorde-rejected` marker as the symmetric counterpart of `.clhorde-ready`. Rejected because the cleanup (delete dir, kill worker, comment Jira) is non-trivial and shouldn't be triggered by a stray file.
- Editing the file by hand. Will keep working — humans can `touch openspec/changes/PROJ-1234/.clhorde-ready` directly. The CLI is the recommended path because of the worker-kill side-effect, but the FS marker remains the source of truth.

### D7 — Worker-pool accounting: explore workers are budgeted

Dormant explore workers consume a slot in `max_workers` like any other interactive worker. We considered exempting them but the cure is worse than the disease: an unbudgeted source can starve the pool from a different angle (claiming "0 worker slots" but holding 30 PTYs).

Mitigations:
- A per-source cap (`[sources.jira] max_concurrent_explore = 5`) that throttles how many explore workers Jira can have alive at once. Excess tickets stay in a Jira-source-internal queue until a slot frees.
- Idle-explore-worker reaping: explore workers with no human input for N hours (default 24h) are killed. The workflow stays in `Exploring`; the human can re-spawn an explore session via `clhorde-cli explore <id>` (this subcommand will reuse `--resume` semantics — same pattern as the TUI's existing `R` resume binding).

### D8 — Jira write-back is opt-in per event class

Three classes:

- **Comments** (low-risk, default on): "🤖 clhorde started exploring this", "🤖 finished — see PR <link>", "🤖 rejected by <user>".
- **Status transitions** (medium-risk, default off): move ticket To Do → In Progress on spawn, In Progress → In Review on archive. Configurable via `[sources.jira.queues.<name>.transitions]`.
- **Label management** (low-risk, default on): remove the trigger label (`clhorde-plan`) when the workflow leaves `Exploring`. Without this, the next poll re-creates a workflow for the same ticket.

All write-back paths must tolerate failure — Jira down should not crash the scheduler or block the workflow's own progress. Failures land as warnings in the daemon log and are visible in the scheduler's `last_jira_error` field.

## Risks / Trade-offs

- **Stale dormant workers** → mitigated by D7's idle reaper. Worth telemetry-ing in v1 so we can tune the default.
- **Jira API down for hours** → poll loop logs warnings, scheduler keeps running, no new Jira-source workflows pick up. Existing workflows are unaffected (they're driven by FS events). The first successful poll after an outage diffs cleanly against in-memory state.
- **The trigger label gets removed externally** → next poll fires `TicketLeftFilter`, the workflow gets cancelled if it was in `Exploring`. Documented behaviour. Some teams will hate it; per-queue config can flip it to "ignore disappearance once exploring has started" if needed.
- **Issue key collision with an OpenSpec change name** → forbidden by validation at workflow creation time. Both sources fight over `name`; first-write wins, second is rejected with a clear error.
- **Prompt template drift** → the literal `/opsx:explore` directive ties us to that skill being installed in the user's Claude Code. Documented as a prerequisite. If the skill is missing the worker will fail; the failure is visible (worker exits non-zero, workflow marked `Failed` with a clear reason).
- **Privacy** → Jira ticket bodies (which may contain customer-internal information) end up in the Claude Code prompt. This is the user's choice; the scheduler doesn't redact. Document loudly.
- **Two-source cooperation** is load-bearing for D2/D3 — if the OpenSpec watcher is misconfigured (e.g., user's `openspec/changes/` is elsewhere), the marker write won't fire any event and the workflow gets stuck. We add a startup sanity check that the configured OpenSpec root exists, and a CLI command to manually re-scan.
- **The `Exploring` state is not actually OpenSpec-specific**, but we're shipping it inside the OpenSpec FSM. If a future GitHub Issues source wants the same gate, they reuse the state — fine, that's why it's named after the activity, not the source. If a future source wants a different gate, a real refactor is on the table.

## Migration Plan

This is a pure addition. No data migration; existing workflows continue to operate unchanged. Rollout is feature-flagged by config: a fresh install ignores Jira entirely until `[sources.jira]` is added.

## Open Questions

1. **Issue type filtering** — should the JQL filter be the only knob, or do we add a separate `issue_types` allowlist? JQL is more flexible but harder to debug. *Lean: JQL only.*
2. **What happens if the human writes `.clhorde-ready` while explore is mid-thinking** (worker is generating tokens)? Current plan: SIGTERM the worker, accept partial artifacts on disk, transition to `Queued`. *Acceptable, but worth a doc note.*
3. **Resume semantics for an exited explore worker** — does `clhorde-cli explore <id>` start a fresh `/opsx:explore` or use `--resume` against the previous session? *Lean: `--resume` to keep history. Falls back to fresh if the session id is unrecoverable.*
4. **Multi-queue priority** — if two queues both match a ticket (rare but possible with bad JQL), do we pick the first, error, or run both? *Lean: error at config validation time, refuse to start with overlapping JQL — but detecting overlap statically is hard, so probably runtime: first queue to see the ticket wins.*
