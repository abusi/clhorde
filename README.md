# clhorde

A daemon+TUI+CLI system for orchestrating multiple Claude Code CLI instances in parallel. Built with Rust, ratatui, and crossterm.

![Rust](https://img.shields.io/badge/rust-2021-orange)

**[Documentation](https://abusi.github.io/clhorde/)**

## Features

- **Daemon architecture** — background daemon (`clhorded`) manages workers; TUI and CLI are thin clients via Unix sockets. Workers survive TUI restarts, multiple clients connect simultaneously
- **Prompt queue + worker pool** — queue unlimited prompts, 1–20 concurrent workers pull automatically
- **Dual architecture** — embedded PTY for interactive, stream-json for one-shot
- **Vim-style modal interface** — Normal, Insert, View, Interact, PtyInteract, Filter modes
- **Batch operations** — select multiple prompts, retry/kill/delete/toggle mode in bulk
- **Prompt tags** — `@tag` syntax for tagging and filtering prompts
- **Git worktree isolation** — per-prompt opt-in with `Ctrl+W`
- **Quick prompts** — single-keypress messages to running workers
- **Multi-line prompt editor** — Shift+Enter for newlines, Ctrl+E to open `$EDITOR`, bracketed paste
- **Prompt templates** — expand `:name` + Tab snippets
- **Batch load from files** — `clhorde prompt-from-files tasks/*.md` to queue prompts from files
- **Session persistence** — prompts saved to disk, resume with `R`
- **Custom keybindings** — fully remappable via TOML config
- **CLI tools** — `submit`, `status`, `attach`, `store`, `keys`, `qp`, `config` subcommands (via `clhorde-cli`), plus `prompt-from-files` (via `clhorde` TUI)

## Install

```bash
git clone https://github.com/abusi/clhorde.git
cd clhorde
cargo build --release
# binaries at target/release/:
#   clhorded     — background daemon (orchestrator)
#   clhorde      — TUI client
#   clhorde-cli  — CLI tool
```

Requires:
- Rust 1.88+
- `claude` CLI installed and in PATH

## Usage

```bash
clhorded &                              # start daemon (background)
clhorde                                 # launch TUI
clhorde-cli status                      # check daemon status
clhorde-cli submit "Review the auth module"  # submit a prompt via CLI
```

The daemon must be running before the TUI or CLI can connect. Press `i` to start typing a prompt. See the [getting started guide](https://abusi.github.io/clhorde/guide.html) for a walkthrough.

### Running clhorded as a systemd user service

To have `clhorded` start automatically on login, install it as a systemd user service.

Create the service file:

```bash
mkdir -p ~/.config/systemd/user
```

Write `~/.config/systemd/user/clhorded.service`:

```ini
[Unit]
Description=clhorde daemon - Claude Code orchestrator
After=default.target

[Service]
ExecStart=/path/to/clhorded
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
```

Replace `/path/to/clhorded` with the actual binary path (e.g. `~/.cargo/bin/clhorded` or `./target/release/clhorded`).

Then enable and start it:

```bash
systemctl --user daemon-reload
systemctl --user enable clhorded.service
systemctl --user start clhorded.service
```

The service will now start automatically each time you log in.

Useful commands:

```bash
systemctl --user status clhorded   # check status
systemctl --user stop clhorded     # stop
systemctl --user restart clhorded  # restart after rebuild
journalctl --user -u clhorded -f   # follow logs
```

## Workflow lifecycle

The scheduler drives each workflow through a small finite state machine. Most states map onto OpenSpec phases the user already knows (`Implementing`, `Verifying`, `Archiving`); the **`Triggered` / `Exploring`** pair was added with the Jira source so a ticket can run `/opsx:explore` interactively before a human approves the resulting proposal.

```
                       Drafted ──queue──▶ Queued ─▶ Implementing ─▶ Verifying ─▶ Archiving ─▶ Archived
                                          ▲              │
                                          │              │
            ┌─────────────────────────────┘              ▼
            │ approval (`.clhorde-ready` / `clhorde-cli approve`)
            │                                         cancel/fail
Triggered ──start_exploring──▶ Exploring                 │
 (Jira)                          │                       ▼
                                 ├─ reject / fail ─▶ Cancelled / Failed { reason }
                                 │
                                 └─ TicketLeftFilter ─▶ Cancelled
```

- **`Drafted`** — change directory exists but no `.clhorde-ready` marker. Default for OpenSpec-source workflows.
- **`Triggered`** — transient. A non-OpenSpec source (today: Jira) just created the workflow but has not yet dispatched the explore worker; promoted to `Exploring` within milliseconds of creation.
- **`Exploring`** — interactive PTY worker is running `/opsx:explore` (or has exited and is waiting to be re-spawned). The workflow is parked here until a human writes `.clhorde-ready` (`clhorde-cli approve <id>`) or rejects (`clhorde-cli reject <id>`). `Exploring` is **not** terminal and **not** "running" in the OpenSpec implementation sense — it is a distinct human-gated parking phase.
- **`Queued` → `Implementing` → `Verifying` → `Archiving` → `Archived`** — the existing OpenSpec phases.
- **`Cancelled` / `Failed { reason }`** — terminal escape hatches.

Each workflow records a `source: SourceKind` (`OpenSpec` or `Jira`) at creation time. The CLI, TUI, and web dashboard all surface the state, source, and a per-source `SourceHealth` (`last_successful_run`, `last_error`, `is_healthy`) snapshot so multi-source operators can see at a glance whether the Jira poll loop is healthy.

## Jira source

The scheduler can ingest work directly from Jira via JQL polling. This is opt-in: a fresh install ignores Jira until `[sources.jira]` is added to `keymap.toml`.

### Required Jira permissions

The Atlassian account whose API token is configured in `auth_token_env` must have the following permissions, scoped to every project the configured JQL filters can reach:

- **Browse Projects** — to read issues that match the JQL filter (`POST /rest/api/2/search`).
- **Add Comments** — to post the `🤖 clhorde started exploring this` / archive / reject comments (`POST /rest/api/2/issue/{key}/comment`). Required when the queue's `comments = true` (default).
- **Transition Issues** — to move a ticket between Jira statuses on workflow lifecycle events (`POST /rest/api/2/issue/{key}/transitions`). Only required when the queue's `transitions` table is non-empty (default: empty / disabled).
- **Edit Issues** — to add and remove the trigger label (`PUT /rest/api/2/issue/{key}` with `update.labels`). Required when the queue's `labels = true` (default).

A typical setup is to provision a dedicated bot account with the project-level role `Developer` (or any custom role that grants the four permissions above). Avoid using a human's personal token: the account name shows up on every comment and transition.

### Auth setup

Generate an Atlassian API token at <https://id.atlassian.com/manage-profile/security/api-tokens>, export it as the env var named in `auth_token_env`, and never commit the token. The scheduler reads the token at request time and never logs it (the in-memory `JiraAuth` redacts the token in `Debug` output).

```bash
export JIRA_API_TOKEN='atlassian-api-token-here'
clhorded
```

If the token env var is unset or empty at startup, the Jira source is registered as unhealthy and other sources continue to run unchanged.

### Configuration schema

Add a `[sources.jira]` block to `~/.config/clhorde/keymap.toml`. A minimal config looks like:

```toml
[sources.jira]
url = "https://your-site.atlassian.net"
email = "bot@your-company.com"          # account tied to the API token
auth_token_env = "JIRA_API_TOKEN"        # env var holding the token
# Optional source-wide knobs:
poll_interval_secs = 30                  # cadence between polls (clamped to a 15s floor)
max_concurrent_explore = 5               # cap on simultaneously active explore workers
idle_explore_timeout = 86400             # seconds before the reaper kills an idle explore worker

[sources.jira.queues.backlog]
filter_jql = "project = PROJ AND labels = clhorde-plan"
mode = "openspec"                        # only "openspec" is implemented today; "direct" is reserved
comments = true                          # post lifecycle comments on the ticket (default: true)
labels = true                            # remove the trigger label on Exploring start (default: true)
trigger_label = "clhorde-plan"           # label removed when the workflow leaves Exploring
# Optional: map workflow lifecycle phases to Jira transition ids. Empty/missing
# disables transitions entirely.
[sources.jira.queues.backlog.transitions]
exploring = "31"
archived = "61"
cancelled = "71"
```

Source-wide fields:

| Field | Required | Default | Notes |
|-------|----------|---------|-------|
| `url` | yes | — | Atlassian site base URL. |
| `email` | yes | — | Account email used as the HTTP Basic username half. |
| `auth_token_env` | yes | — | Env var the token is read from. |
| `poll_interval_secs` | no | 30 | Clamped at runtime to a 15-second floor (a warning is logged if you go lower). |
| `max_concurrent_explore` | no | 5 | Per-source cap on parked explore workers; excess tickets queue inside the source. |
| `idle_explore_timeout` | no | 86400 | Seconds before the explore-worker reaper kills a parked worker. |

Per-queue fields (`[sources.jira.queues.<name>]`):

| Field | Required | Default | Notes |
|-------|----------|---------|-------|
| `filter_jql` | yes | — | Raw JQL passed verbatim to Jira's `/search` endpoint. |
| `mode` | no | `"openspec"` | Only `"openspec"` is implemented today. `"direct"` is reserved for a follow-up change; queues using it are skipped at startup with a clear error. |
| `comments` | no | `true` | Post `🤖 clhorde …` comments on lifecycle events. |
| `labels` | no | `true` | Remove the trigger label when the workflow leaves the explore gate. |
| `trigger_label` | no | `"clhorde-plan"` | Label removed by the labels-write-back step. |
| `transitions` | no | `{}` | Map from lifecycle phase (`"exploring"`, `"archived"`, `"cancelled"`) to Jira transition id. Empty disables transitions entirely. |

Validation runs at scheduler startup. Source-wide errors (missing `url`/`email`/`auth_token_env`, `max_concurrent_explore = 0`) disable the whole source. Per-queue errors (missing `filter_jql`, `mode = "direct"`, unrecognised mode, etc.) skip just that queue with a logged warning; other queues in the same source keep running.

## Documentation

Full documentation is available at **[abusi.github.io/clhorde](https://abusi.github.io/clhorde/)**:

- [Getting Started](https://abusi.github.io/clhorde/guide.html)
- [Features](https://abusi.github.io/clhorde/features.html)
- [Keybindings](https://abusi.github.io/clhorde/keybindings.html)
- [Configuration](https://abusi.github.io/clhorde/configuration.html)
- [CLI Reference](https://abusi.github.io/clhorde/cli.html)
- [Cheatsheet](https://abusi.github.io/clhorde/cheatsheet.html)

## License

MIT
