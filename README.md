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
