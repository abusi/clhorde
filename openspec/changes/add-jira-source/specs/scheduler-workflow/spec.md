## ADDED Requirements

### Requirement: Workflow lifecycle includes an `Exploring` state

The scheduler workflow FSM SHALL include an `Exploring` state, occupied by workflows whose proposal artifacts are being authored interactively (via `/opsx:explore`) and which are awaiting a human approval signal (a `.clhorde-ready` marker write or `clhorde-cli approve`).

#### Scenario: Exploring is non-terminal
- **WHEN** a workflow is in `Exploring`
- **THEN** it is neither terminal (no `is_terminal()`) nor "running" in the sense of OpenSpec implementation phases (no `is_running()`); it is a distinct gating phase

#### Scenario: Exploring persists across daemon restart
- **GIVEN** workflow `PROJ-1` is in `Exploring`
- **WHEN** the daemon is restarted
- **THEN** the workflow is still in `Exploring` after startup; its previous PTY worker (if any) is gone, but the workflow itself is recovered from persistence

### Requirement: Allowed transitions into and out of `Exploring`

The scheduler SHALL allow the following transitions and SHALL reject all others with a `TransitionError`:

- **into Exploring**: from `Triggered` (when an `explore-gate`-using source has dispatched a worker for it).
- **out of Exploring**:
  - to `Queued` — driven by the existing `MarkerCreated` handler, unchanged from current OpenSpec semantics.
  - to `Cancelled` — driven by `RejectRequested` (CLI signal) OR by `TicketLeftFilter` (Jira-source-emitted, while in `Exploring` only).
  - to `Failed { reason }` — driven by catastrophic worker dispatch failure.

#### Scenario: MarkerCreated transitions Exploring to Queued
- **GIVEN** workflow `PROJ-1` is in `Exploring`
- **WHEN** the OpenSpec watcher emits `MarkerCreated { name: "PROJ-1" }`
- **THEN** the orchestrator transitions `PROJ-1` to `Queued` (same handler that handles transitions from `Drafted` to `Queued`)

#### Scenario: Cannot transition Exploring directly to Implementing
- **GIVEN** workflow `PROJ-1` is in `Exploring`
- **WHEN** the orchestrator attempts to call `start_implementing()`
- **THEN** the call returns `TransitionError`; the workflow stays in `Exploring`

#### Scenario: Cannot enter Exploring from Implementing
- **GIVEN** workflow `PROJ-1` is in `Implementing`
- **WHEN** the orchestrator attempts to transition it to `Exploring`
- **THEN** the call returns `TransitionError`; the workflow stays in `Implementing`

### Requirement: Workflows carry a source identifier

Each workflow SHALL record which source created it (`source: SourceKind`). The variants SHALL include `OpenSpec` and `Jira`. The field SHALL be persisted alongside other workflow fields, with a `Default` of `OpenSpec` for backward compatibility with workflows persisted before this change.

#### Scenario: Jira-created workflow is tagged
- **WHEN** the Jira source creates workflow `PROJ-1`
- **THEN** `PROJ-1.source == SourceKind::Jira`

#### Scenario: Pre-existing workflows default to OpenSpec
- **GIVEN** a persisted workflow file written before this change (no `source` field)
- **WHEN** the daemon loads it on startup
- **THEN** the in-memory workflow has `source == SourceKind::OpenSpec`

### Requirement: Workflow names are unique across sources

The orchestrator SHALL refuse to create a workflow whose name collides with an existing workflow's name, regardless of source. The conflict SHALL be surfaced as a clear error to the source attempting creation; the existing workflow SHALL be unaffected.

#### Scenario: Jira ticket key collides with OpenSpec change name
- **GIVEN** workflow `PROJ-1234` exists from the OpenSpec source
- **WHEN** the Jira source attempts to create a workflow named `PROJ-1234`
- **THEN** creation fails; the OpenSpec workflow is unchanged; an error is logged identifying both sources

### Requirement: `Exploring` is surfaced in CLI/TUI/Web

The new state SHALL appear in the scheduler's status surface alongside existing states (`Drafted`, `Queued`, `Implementing`, …). It SHALL be distinguishable in `clhorde-cli status`, the TUI workflow list, and the web dashboard.

#### Scenario: CLI status shows Exploring
- **GIVEN** workflow `PROJ-1` is in `Exploring`
- **WHEN** the user runs `clhorde-cli status` (or its scheduler-side equivalent)
- **THEN** the output identifies `PROJ-1` as `Exploring`, distinct from `Drafted` and `Queued`

#### Scenario: TUI shows Exploring with a distinct rendering
- **GIVEN** workflow `PROJ-1` is in `Exploring`
- **WHEN** the user opens the TUI workflow list
- **THEN** `PROJ-1`'s row is rendered with a state label corresponding to `Exploring`
