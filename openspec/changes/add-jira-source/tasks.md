## 1. Workflow FSM extension

- [x] 1.1 Add `WorkflowStatus::Triggered` and `WorkflowStatus::Exploring` variants to `clhorde-scheduler::workflow`
- [x] 1.2 Add `SourceKind { OpenSpec, Jira }` enum and `Workflow.source` field with `serde(default)` for backward-compatible loading
- [x] 1.3 Implement `start_exploring()`, `cancel_from_exploring()`, and approval-driven `Exploring → Queued` transitions; reject all other transitions out of `Exploring`
- [x] 1.4 Update `Workflow::is_terminal()` and `is_running()` semantics; add tests for `Exploring` (non-terminal, non-running)
- [x] 1.5 Update FSM unit tests to cover all allowed/disallowed transitions in the new state
- [x] 1.6 Update persistence round-trip tests to verify `Exploring` and `source` survive disk + reload
- [x] 1.7 Update CLI/TUI/Web rendering for the new state (status string, color, blocked_by panel where relevant)

## 2. Multi-source plumbing

- [x] 2.1 Refactor orchestrator's event dispatch to accept a unified event type (or a small enum wrapping `FsEvent | JiraEvent`); keep `handle_event(FsEvent)` as a delegate for now
- [x] 2.2 Add `SourceHealth { last_successful_run, last_error, is_healthy }` per source and surface it in the daemon's status response
- [x] 2.3 Add a workflow-name uniqueness check at creation; return a typed error usable by both sources
- [x] 2.4 Wire OpenSpec source registration through the same path Jira will use, so both sources go through one startup helper
- [x] 2.5 Add tests covering Jira-and-OpenSpec coexistence (event from one source advances a workflow created by the other)

## 3. Jira REST client

- [x] 3.1 Add a Jira client module under `crates/clhorde-scheduler/src/jira/` with a thin async wrapper (search, comment, transition, label add/remove)
- [x] 3.2 Use `reqwest` directly (already in the dep tree if practical) or evaluate `gouqi`; prefer minimum surface
- [x] 3.3 Read auth (token + email) from env per `auth_token_env` config; never log the token
- [x] 3.4 Implement exponential backoff on 429s and 5xx; surface `last_error` in source health
- [x] 3.5 Mock-server tests covering success, 401, 429, 500, and offline (connection refused)
- [x] 3.6 Document required Jira permissions (read issues, comment, transition, manage labels) in README

## 4. Jira source — polling and event emission

- [x] 4.1 Add `crates/clhorde-scheduler/src/jira/source.rs` with the poll loop scaffold
- [x] 4.2 Implement per-queue JQL polling at `poll_interval_secs` (clamped to ≥15s) with in-memory diff against last-seen issue keys
- [x] 4.3 Emit `JiraEvent::TicketAppeared { key, payload }` and `JiraEvent::TicketLeftFilter { key }` to the orchestrator's event channel
- [x] 4.4 Persist last-seen keys per queue across daemon restarts (so a restart doesn't re-comment on every existing ticket); keep persisted state cheap and recoverable from a stale snapshot
- [x] 4.5 Tests: synthetic Jira responses driving event emission; ticket-appears, ticket-leaves, ticket-stable, network-down

## 5. Explore gate

- [x] 5.1 Add `crates/clhorde-scheduler/src/explore/` (or fold into the source module if it stays small) with the explore-gate dispatch helper
- [x] 5.2 Implement the prompt template (literal string in source) including the change-name directive and ticket payload substitution
- [x] 5.3 Dispatch the explore worker as `Prompt::interactive` via the existing daemon IPC; on success transition `Triggered → Exploring`
- [x] 5.4 Implement `RejectRequested` event handling: kill worker, remove change directory, transition to `Cancelled`, notify origin source for write-back
- [x] 5.5 Implement `MarkerCreated` interception while in `Exploring`: kill the explore worker if alive, then run the existing `Drafted → Queued` handler logic
- [x] 5.6 Implement idle-explore-worker reaper (background task; default threshold 24h; configurable)
- [x] 5.7 Implement per-source explore concurrency cap (`max_concurrent_explore`) with a small in-memory queue inside the Jira source
- [x] 5.8 Tests: dispatch happy-path, marker-during-explore kills worker, reject mid-explore cleans up, worker exits without marker leaves workflow Exploring, reaper kills idle worker without state change

## 6. Jira write-back

- [x] 6.1 Implement comment posting on lifecycle events (`Exploring` start, `Archived`, `Cancelled`) gated by `comments` config flag (default on)
- [x] 6.2 Implement label remove on `Exploring` start (so re-poll doesn't re-trigger) gated by `labels` config flag (default on)
- [x] 6.3 Implement optional status transitions (`transitions` config, default off) — map workflow states to per-queue transition ids
- [x] 6.4 Wrap all write-back calls in a fire-and-forget task: log on failure, update `last_jira_error`, never block the orchestrator
- [x] 6.5 Tests: comment-fail does not block archive, transitions-disabled means no transition call is issued, label-remove is best-effort

## 7. CLI subcommands

- [x] 7.1 Add `clhorde-cli approve <id>` routed through daemon IPC; daemon handler writes the marker and kills the explore worker atomically
- [x] 7.2 Add `clhorde-cli reject <id>` routed through daemon IPC; daemon handler kills worker, removes change dir, transitions to `Cancelled`, fires source notification
- [x] 7.3 Add `clhorde-cli explore <id>` to (re)start an explore session for a workflow already in `Exploring`; uses `--resume` when session id is recoverable, else fresh `/opsx:explore`
- [x] 7.4 Wire `approve` / `reject` / `explore` errors with clear messages for "no such workflow", "wrong state", "missing change dir"
- [x] 7.5 Tests: approve happy path, approve on wrong-state workflow, reject happy path, reject when Jira write-back fails, explore-resume happy path, explore-fresh fallback

## 8. Configuration

- [x] 8.1 Extend `keymap.toml` schema with `[sources.jira]` (url, auth_token_env, poll_interval_secs, max_concurrent_explore, idle_explore_timeout)
- [x] 8.2 Extend with `[sources.jira.queues.<name>]` tables (filter_jql, mode, comments, labels, transitions table)
- [x] 8.3 Validate at scheduler startup; refuse to start with `mode = "direct"` queues active (clear error pointing to follow-up change)
- [x] 8.4 Document the schema in README and provide an example block in `keymap_example.toml`
- [x] 8.5 Tests: round-trip parse, default values, missing required fields, unrecognised mode

## 9. Surface and UX

- [x] 9.1 Render `Exploring` state in CLI status (color, label, optional "worker_alive" indicator)
- [x] 9.2 Render `Exploring` state in TUI workflow list with a distinct symbol; surface `source` in the list
- [x] 9.3 Render `Exploring` state in web dashboard; add a per-workflow action area for approve/reject (UI is optional in this change — CLI is enough — but keep the surface honest)
- [x] 9.4 Surface `SourceHealth` in CLI/TUI/web (a small section per source: last_run, last_error)
- [x] 9.5 Document the new state in CLAUDE.md and the README state diagram

## 10. End-to-end validation

- [ ] 10.1 Manual test: spin up daemon with Jira config pointing at a sandbox project; trigger a real ticket; attach via TUI; converse; approve; observe `Implementing` and Jira write-back
- [ ] 10.2 Manual test: same flow but reject; verify directory cleanup and Jira comment + label-remove
- [ ] 10.3 Manual test: same flow but ticket-left-filter mid-explore; verify cancellation
- [ ] 10.4 Manual test: daemon restart while workflow is `Exploring`; verify state recovers and `clhorde-cli explore <id>` resumes the conversation
- [ ] 10.5 Update `CLAUDE.md` once stable
