## Why

The scheduler today only ingests work from one source: OpenSpec change directories signalled by a `.clhorde-ready` marker on the local filesystem. Most teams still file work in Jira, and asking them to mirror every ticket into an OpenSpec change is friction-heavy and won't happen. Adding a Jira source lets the scheduler pick up work where it already lives, without forcing teams to adopt OpenSpec end-to-end. The "human-in-the-middle" model — scheduler runs `/opsx:explore` on the ticket, parks the workflow until a human writes `.clhorde-ready` — keeps planning quality high while removing the manual "copy ticket into OpenSpec" step.

## What Changes

- Add a Jira source to the scheduler that polls one or more JQL filters and turns matching issues into scheduler workflows keyed by the issue key (e.g. `PROJ-1234`).
- Add a new `Exploring` workflow state for issues whose worker is still running `/opsx:explore` and waiting on a human.
- Spawn an interactive (PTY) worker per Jira-triggered ticket, seeded with `/opsx:explore` and the ticket payload. The worker is dormant until a human attaches via TUI/web; once they finish the conversation and write `.clhorde-ready`, the existing OpenSpec source resumes the workflow unchanged.
- Add `clhorde-cli approve <id>` and `clhorde-cli reject <id>` subcommands that write/remove the marker, kill any live explore worker, and (for reject) clean up the partial change directory.
- Add Jira write-back: comment on the ticket when an explore worker is spawned, when the workflow reaches a terminal state, and when it is rejected. Optionally transition the ticket between Jira statuses (off by default).
- Add config support under `[sources.jira]` in `keymap.toml`, including auth, polling cadence, and one or more named queues each with their own JQL filter and `mode` (`direct` | `openspec`).
- Multiple sources now coexist; workflow names must remain globally unique. The Jira source uses the issue key directly so collision with OpenSpec change names is the user's problem (and easy to avoid).

Non-goals for this change:
- Subtask DAGs, AC-checklist parsing, or any structured decomposition of ticket bodies. The first cut treats the ticket as one prompt blob.
- Direct mode (`mode = "direct"`) is wired through config but the implementation only ships `mode = "openspec"`. Direct mode lands in a follow-up change.
- A bespoke "review proposals" UI in TUI or web. Humans review by reading the artifacts in their editor and using the `approve` / `reject` CLI.
- Multi-Jira-instance support. One Atlassian site per scheduler instance.

## Capabilities

### New Capabilities
- `jira-source`: scheduler ingestion of Jira issues via JQL polling, with per-queue `mode` selection and write-back on workflow lifecycle events.
- `explore-gate`: a workflow lifecycle phase where the scheduler runs `/opsx:explore` in an interactive worker and parks the workflow until a human writes `.clhorde-ready`. Source-agnostic in principle — Jira is the first user.
- `scheduler-workflow`: the workflow FSM and its lifecycle states. This change is the first time it is captured spec-side; the addition of `Exploring` is the immediate motivator.
- `scheduler-source`: the multi-source architecture, including the contract that lets multiple sources cooperate on one workflow's lifecycle through the `.clhorde-ready` marker hand-off.

### Modified Capabilities
<!-- None — `openspec/specs/` is empty, so there are no existing capability specs to delta against. The four capabilities above are all spec-side new even when their underlying code partly exists today. -->


## Impact

- **`crates/clhorde-scheduler/`** — new `jira/` module sibling to `openspec/`; new event variants flowing into the orchestrator; `Workflow` gains `Exploring` and source-tagging fields.
- **`crates/clhorde-cli/`** — new `approve` / `reject` subcommands.
- **`crates/clhorde-core/`** — config schema additions for `[sources.jira]`.
- **External dependencies** — a Jira REST client. `gouqi` or a thin `reqwest`-based client.
- **Secrets** — Jira API token in env / config. Documented in README; never persisted by the daemon.
- **Operational surface** — a new long-running poll loop in the scheduler, with backoff and visible health (last-poll timestamp, last-error). New CLI commands. New Jira-side activity (comments, optional transitions).
- **Out-of-scope for this change but on the radar** — direct mode (`mode = "direct"`), multi-instance Jira, GitHub Issues source as a parallel implementation.
