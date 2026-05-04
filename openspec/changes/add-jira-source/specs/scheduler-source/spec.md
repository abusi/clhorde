## ADDED Requirements

### Requirement: The scheduler supports multiple cooperating sources

The scheduler SHALL support running multiple sources concurrently. Each source produces lifecycle events for workflows; the orchestrator consumes events from all sources through a unified handler interface. Sources SHALL NOT need to know about each other to function correctly.

#### Scenario: OpenSpec and Jira sources run side-by-side
- **GIVEN** the scheduler is configured with both OpenSpec and Jira sources active
- **WHEN** both sources emit events for distinct workflows
- **THEN** each workflow advances independently, driven only by events from sources relevant to its current state

#### Scenario: A single workflow can be advanced by multiple sources across its lifetime
- **GIVEN** workflow `PROJ-1` was created by the Jira source (state `Exploring`)
- **WHEN** the OpenSpec source's filesystem watcher later observes `.clhorde-ready` under `openspec/changes/PROJ-1/`
- **THEN** the OpenSpec source's `MarkerCreated` event advances `PROJ-1` to `Queued`; no Jira-specific code runs in that transition

### Requirement: Source-emitted events flow through a single orchestrator entry point

All source-emitted lifecycle events SHALL flow into the orchestrator through a single dispatch entry point. The existing `handle_event(FsEvent)` MAY be renamed or generalised, but the requirement is one entry point per workflow event regardless of origin source.

#### Scenario: Jira event is processed by the same orchestrator
- **WHEN** the Jira source emits `JiraEvent::TicketAppeared { key, payload }`
- **THEN** the event is processed by the same orchestrator instance that processes `FsEvent`s, with no per-source branching at the call site

### Requirement: Source health is observable

The scheduler SHALL expose per-source health via its existing status surface. Each source SHALL report at minimum:

- `last_successful_run` (timestamp of last successful poll/scan/event accept)
- `last_error` (most recent error message, if any)
- `is_healthy` (boolean: no errors since last success)

#### Scenario: Source health visible in CLI
- **GIVEN** the Jira source has not been able to reach the API for 5 minutes due to network errors
- **WHEN** the user runs `clhorde-cli status` (scheduler-side)
- **THEN** the output reports the Jira source's `last_error` and shows it as unhealthy

### Requirement: Source configuration is namespaced

Each source's configuration SHALL live under a distinct top-level key in `keymap.toml` (`[sources.<source-name>]` and nested tables). Adding or removing a source's config SHALL NOT affect other sources.

#### Scenario: Removing Jira config leaves OpenSpec source running
- **GIVEN** a configuration with both `[sources.openspec]` and `[sources.jira]` sections
- **WHEN** the user removes the `[sources.jira]` section and restarts the scheduler
- **THEN** the OpenSpec source continues to run unchanged; the Jira source is not registered; no errors are logged about the missing Jira config

### Requirement: A source MAY observe but MUST NOT block workflows it did not originate

A source SHALL be free to observe lifecycle events (e.g., for write-back) for workflows it did not originate, but SHALL NOT prevent or delay state transitions. Side-effects (Jira comments, status transitions, etc.) MUST be issued asynchronously relative to the FSM advance.

#### Scenario: Jira write-back lag does not delay OpenSpec workflow archive
- **GIVEN** workflow `PROJ-1` (created by Jira) reaches `Archived`
- **WHEN** the scheduler attempts to post a closing Jira comment that takes 10 seconds
- **THEN** the workflow's transition to `Archived` is observable immediately in the orchestrator (CLI/TUI/web see it instantly); the Jira comment posts asynchronously

### Requirement: Source startup ordering is independent of source set

The scheduler SHALL start each configured source independently. A source that fails to initialise (bad config, unreachable API at boot) SHALL log a clear error and be marked unhealthy, but SHALL NOT prevent other sources from starting.

#### Scenario: Jira API unreachable at boot does not block scheduler
- **GIVEN** the Jira API is unreachable at scheduler startup
- **WHEN** the scheduler boots
- **THEN** the OpenSpec source starts normally and the scheduler accepts CLI/TUI connections; the Jira source is registered as unhealthy with the boot-time error visible
