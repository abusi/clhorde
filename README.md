# clhorde

A lightweight, single-binary TUI for orchestrating multiple Claude Code CLI instances in parallel. Built with Rust, ratatui, and crossterm.

![Rust](https://img.shields.io/badge/rust-2021-orange)

**[Documentation](https://your-user.github.io/clhorde/)**

## Features

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
- **CLI management** — `store`, `keys`, `qp`, `config`, `prompt-from-files` subcommands
- **Zero dependencies** — single binary, just needs `claude` in PATH

## Install

```bash
git clone https://github.com/your-user/clhorde.git
cd clhorde
cargo build --release
# binary is at target/release/clhorde
```

Requires:
- Rust 1.88+
- `claude` CLI installed and in PATH

## Usage

```bash
clhorde              # launch TUI
clhorde --help       # show help
```

Press `i` to start typing a prompt. See the [getting started guide](https://your-user.github.io/clhorde/guide.html) for a walkthrough.

## Documentation

Full documentation is available at **[your-user.github.io/clhorde](https://your-user.github.io/clhorde/)**:

- [Getting Started](https://your-user.github.io/clhorde/guide.html)
- [Features](https://your-user.github.io/clhorde/features.html)
- [Keybindings](https://your-user.github.io/clhorde/keybindings.html)
- [Configuration](https://your-user.github.io/clhorde/configuration.html)
- [CLI Reference](https://your-user.github.io/clhorde/cli.html)
- [Cheatsheet](https://your-user.github.io/clhorde/cheatsheet.html)

## License

MIT
