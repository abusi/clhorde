## ADDED Requirements

### Requirement: Explore gate spawns an interactive worker seeded with `/opsx:explore`

When a source decides a workflow needs human-gated proposal authoring, the scheduler SHALL dispatch an interactive (PTY-mode) worker whose initial prompt invokes `/opsx:explore` and embeds the source-supplied payload (e.g., a Jira ticket body). The workflow SHALL transition into `Exploring` once dispatch succeeds.

#### Scenario: Worker is interactive, not one-shot
- **WHEN** the explore gate dispatches a worker for workflow `PROJ-1`
- **THEN** the resulting prompt has `mode == Interactive`, runs in a real PTY, and survives until externally terminated or until the human exits the session

#### Scenario: Prompt template carries the change-name directive
- **WHEN** the explore gate dispatches a worker for workflow `PROJ-1`
- **THEN** the worker's prompt contains a directive instructing the AI to use `PROJ-1` as the change directory name

#### Scenario: Worker dispatch failure transitions the workflow to Failed
- **GIVEN** the daemon refuses prompt submission (e.g., daemon socket unavailable)
- **WHEN** the explore gate attempts to dispatch the worker
- **THEN** the workflow transitions to `Failed { reason }` with a reason naming the dispatch failure

### Requirement: Workflow stays in `Exploring` until approved or rejected

A workflow in `Exploring` SHALL remain in `Exploring` regardless of whether its explore worker is currently alive. Exits from `Exploring` are exactly:

1. `MarkerCreated` for the workflow's name → `Queued`.
2. `RejectRequested` (CLI signal) → `Cancelled`.
3. `TicketLeftFilter` (source-emitted) → `Cancelled`.
4. Catastrophic dispatch failure → `Failed`.

Worker exit on its own is NOT an exit from `Exploring`.

#### Scenario: Worker exits without marker — workflow stays Exploring
- **GIVEN** workflow `PROJ-1` is in `Exploring` and the PTY worker has exited cleanly
- **WHEN** no marker has been written
- **THEN** the workflow remains in `Exploring`; the change directory state on disk is unchanged

#### Scenario: Marker written while worker is alive — worker is killed
- **GIVEN** workflow `PROJ-1` is in `Exploring` with a live PTY worker
- **WHEN** `.clhorde-ready` is written under `openspec/changes/PROJ-1/`
- **THEN** the worker receives SIGTERM (graceful), the workflow transitions to `Queued`, and the existing OpenSpec source's marker handler runs unchanged

#### Scenario: Reject during exploration kills the worker
- **GIVEN** workflow `PROJ-1` is in `Exploring` with a live PTY worker
- **WHEN** `clhorde-cli reject PROJ-1` is invoked
- **THEN** the worker is killed, `openspec/changes/PROJ-1/` is removed, the workflow transitions to `Cancelled`, and the source is notified for write-back

### Requirement: `clhorde-cli approve` writes the marker via the daemon

The `clhorde-cli approve <id>` subcommand SHALL route through the daemon (over the existing IPC channel) to write `openspec/changes/<id>/.clhorde-ready` and gracefully kill any live explore worker for that workflow. Direct user `touch` of the marker file SHALL also work, but the CLI is the documented happy path because it carries the worker-kill side-effect atomically.

#### Scenario: Approve writes marker and kills worker
- **GIVEN** workflow `PROJ-1` is `Exploring` with a live worker
- **WHEN** the user runs `clhorde-cli approve PROJ-1`
- **THEN** `.clhorde-ready` exists in the change directory, the worker is gone, and the workflow is `Queued`

#### Scenario: Approve fails for a non-Exploring workflow
- **GIVEN** workflow `PROJ-1` is `Implementing`
- **WHEN** the user runs `clhorde-cli approve PROJ-1`
- **THEN** the command exits non-zero with a clear error; no marker is written; the workflow is unchanged

#### Scenario: Approve fails when no change directory exists
- **GIVEN** workflow `PROJ-1` is `Exploring` but the worker never created `openspec/changes/PROJ-1/`
- **WHEN** the user runs `clhorde-cli approve PROJ-1`
- **THEN** the command exits non-zero with an error explaining the missing directory; the workflow remains in `Exploring`

### Requirement: `clhorde-cli reject` cleans up and notifies the source

The `clhorde-cli reject <id>` subcommand SHALL: kill the live explore worker if any; remove `openspec/changes/<id>/` if it exists; transition the workflow to `Cancelled`; and emit a source-side notification (e.g., the Jira source posts a comment and removes the trigger label). All side-effects SHALL be best-effort — failure of any individual step SHALL be logged but SHALL NOT prevent the workflow's local cancellation.

#### Scenario: Reject fully cleans up
- **GIVEN** workflow `PROJ-1` is `Exploring` with a worker and a partially-written `openspec/changes/PROJ-1/proposal.md`
- **WHEN** the user runs `clhorde-cli reject PROJ-1`
- **THEN** the worker is killed, the directory is removed, the workflow is `Cancelled`, and the Jira source is notified

#### Scenario: Reject succeeds even if Jira write-back fails
- **GIVEN** workflow `PROJ-1` is `Exploring` and Jira is down
- **WHEN** the user runs `clhorde-cli reject PROJ-1`
- **THEN** the local cancellation succeeds, the worker and directory are cleaned up, and the Jira write-back failure is logged with `last_jira_error` updated

### Requirement: Resuming a closed explore session is supported

Once a workflow is in `Exploring` and its previous explore worker has exited, the user SHALL be able to start a fresh explore session for the same workflow via `clhorde-cli explore <id>`. The new session SHOULD use `--resume` semantics to continue the prior conversation when possible, and SHALL fall back to a fresh `/opsx:explore` invocation when the prior session id is unrecoverable.

#### Scenario: Resume picks up prior conversation
- **GIVEN** workflow `PROJ-1` is in `Exploring`, its prior worker has exited, and its session id is recoverable
- **WHEN** the user runs `clhorde-cli explore PROJ-1`
- **THEN** a new PTY worker is dispatched with `--resume <session-id>` and the prior conversation history is visible in the worker's terminal

#### Scenario: Resume falls back to fresh explore
- **GIVEN** workflow `PROJ-1` is in `Exploring`, its prior worker has exited, and the session id is missing or invalid
- **WHEN** the user runs `clhorde-cli explore PROJ-1`
- **THEN** a fresh `/opsx:explore` worker is dispatched seeded with the original ticket payload

### Requirement: Idle explore workers are subject to reaping

The scheduler MAY reap an explore worker that has had no human input for an idle threshold (configurable per source, default 24 hours). Reaping SHALL kill the worker process but MUST NOT modify the workflow's state, the change directory, or the marker.

#### Scenario: Reaper does not advance the FSM
- **GIVEN** an explore worker has been idle past the threshold
- **WHEN** the reaper kills it
- **THEN** the workflow remains in `Exploring`; the change directory and any partial artifacts are preserved
