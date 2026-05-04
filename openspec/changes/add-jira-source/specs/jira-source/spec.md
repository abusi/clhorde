## ADDED Requirements

### Requirement: Jira source polls configured JQL queues at a fixed cadence

The scheduler SHALL poll each configured Jira queue (`[sources.jira.queues.<name>]`) on an interval no shorter than 15 seconds and no longer than `poll_interval_secs` (default 30 seconds). Each poll SHALL execute the queue's `filter_jql` against the configured Atlassian site and produce, for that queue, the set of issue keys currently matching the filter.

#### Scenario: Initial poll discovers tickets
- **WHEN** the scheduler starts with a configured Jira queue and the JQL filter matches issue `PROJ-1`
- **THEN** within `poll_interval_secs + 5s` of startup, a workflow named `PROJ-1` exists in the orchestrator with `source: SourceKind::Jira` and status `Exploring`

#### Scenario: Subsequent poll for already-known ticket is a no-op
- **WHEN** issue `PROJ-1` matches the filter on a poll, and a workflow named `PROJ-1` already exists in the orchestrator
- **THEN** the workflow is unchanged; no new prompt is dispatched; no new Jira comment is posted

#### Scenario: Poll runs no more often than the configured floor
- **WHEN** the configured `poll_interval_secs` is set to 5
- **THEN** the scheduler clamps it to 15 and logs a warning at startup; subsequent polls run no more often than every 15 seconds

### Requirement: A ticket leaving the filter cancels its in-flight workflow

When a Jira poll reveals that an issue key previously matched but no longer does, AND the corresponding workflow is in `Exploring`, the scheduler SHALL transition the workflow to `Cancelled` and clean up resources (kill any live worker, remove partial change directory).

#### Scenario: External actor removes the trigger label
- **GIVEN** workflow `PROJ-2` is in `Exploring` and the trigger label was removed in Jira
- **WHEN** the next poll runs and `PROJ-2` is no longer in the filter
- **THEN** the workflow transitions to `Cancelled`, the live PTY worker (if any) receives SIGTERM, and `openspec/changes/PROJ-2/` is removed

#### Scenario: Ticket leaves filter after workflow already advanced past Exploring
- **GIVEN** workflow `PROJ-3` is in `Implementing`
- **WHEN** the next poll shows `PROJ-3` no longer matches the filter
- **THEN** the workflow continues unchanged; cancel-on-leave applies only while in `Exploring`

### Requirement: Jira-triggered workflows are keyed by issue key

The scheduler SHALL use the Jira issue key verbatim as the workflow name. The associated change directory SHALL be `openspec/changes/<KEY>/`.

#### Scenario: Workflow name equals issue key
- **WHEN** issue `PROJ-1234` is picked up by the Jira source
- **THEN** the resulting workflow has `name == "PROJ-1234"` and writes to `openspec/changes/PROJ-1234/`

#### Scenario: Collision with existing OpenSpec change is rejected
- **GIVEN** a workflow named `PROJ-1234` already exists from the OpenSpec source
- **WHEN** the Jira source picks up issue `PROJ-1234`
- **THEN** workflow creation fails with a clear error logged; the Jira ticket is not modified

### Requirement: Jira write-back classes are independently configurable

The scheduler SHALL support three classes of Jira write-back, each independently togglable per queue:

- **comments** (default: on) — short comments on lifecycle events.
- **transitions** (default: off) — Jira status transitions on workflow state changes.
- **labels** (default: on) — adding/removing the trigger label on workflow state changes.

#### Scenario: Comment is posted when explore worker spawns
- **GIVEN** queue config has `comments = true`
- **WHEN** workflow `PROJ-1` enters `Exploring`
- **THEN** a Jira comment "🤖 clhorde started exploring this" (or equivalent templated text) is posted on `PROJ-1`

#### Scenario: Comments disabled means no Jira chatter
- **GIVEN** queue config has `comments = false`
- **WHEN** workflow `PROJ-1` enters any lifecycle state
- **THEN** no comment is posted on `PROJ-1`

#### Scenario: Status transition only when explicitly enabled
- **GIVEN** queue config has `transitions = false`
- **WHEN** workflow `PROJ-1` reaches `Archived`
- **THEN** the Jira ticket's status is unchanged

### Requirement: Jira write-back failures do not block workflow progress

If a Jira API call (comment, transition, label) fails, the scheduler SHALL log a warning, expose the failure via `last_jira_error` in source health, and proceed with the workflow's local state machine unchanged.

#### Scenario: Comment fails on archive
- **GIVEN** Jira is returning 500s
- **WHEN** workflow `PROJ-1` reaches `Archived` and the scheduler attempts to post the closing comment
- **THEN** the comment attempt fails, a warning is logged, `last_jira_error` is updated, and the workflow remains `Archived`

#### Scenario: Auth expired
- **GIVEN** the configured Jira API token is invalid
- **WHEN** any poll runs
- **THEN** poll attempts return Unauthorized errors that are logged and surfaced via source health; the scheduler does not crash, and existing workflows continue running

### Requirement: Jira source mode `direct` is reserved but not implemented

The configuration schema SHALL accept `mode = "direct"` for a queue, but the scheduler SHALL refuse to start with such a queue active in this change, returning a clear "not yet implemented" error.

#### Scenario: Direct mode rejected at startup
- **GIVEN** `[sources.jira.queues.foo] mode = "direct"`
- **WHEN** the scheduler starts
- **THEN** it logs a clear error pointing to a follow-up change and refuses to register that queue (other queues with `mode = "openspec"` continue to work)

### Requirement: Per-source explore worker cap

The scheduler SHALL respect a `[sources.jira] max_concurrent_explore` setting (default 5) limiting how many `Exploring` workflows may have a live worker at one time. When the cap is reached, additional matching tickets stay queued internally and become `Triggered → Exploring` only when a slot frees.

#### Scenario: Cap holds excess tickets
- **GIVEN** `max_concurrent_explore = 2` and three matching tickets `PROJ-1`, `PROJ-2`, `PROJ-3`
- **WHEN** all three appear in the same poll
- **THEN** two of them have a live explore worker dispatched; the third is held in the source's internal queue and dispatched as soon as one of the two frees a slot

### Requirement: Idle explore workers are reaped

The scheduler SHALL kill any explore worker that has gone `idle_explore_timeout` (default 24h, configurable per source) without receiving human input. The associated workflow SHALL remain in `Exploring`; partial artifacts (if any) SHALL remain on disk.

#### Scenario: Idle worker reaped
- **GIVEN** workflow `PROJ-1` is in `Exploring` with a live PTY worker that has had no input for 24h+1m
- **WHEN** the reaper runs
- **THEN** the worker is killed; the workflow stays in `Exploring`; the change directory is untouched

#### Scenario: Active worker is not reaped
- **GIVEN** workflow `PROJ-2` is in `Exploring` with a worker that received human keystrokes within the last hour
- **WHEN** the reaper runs
- **THEN** the worker is left running
