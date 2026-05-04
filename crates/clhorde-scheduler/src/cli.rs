//! CLI surface for `clhorde-scheduler`.
//!
//! The argument types live here (rather than in `main.rs`) so we can unit-test
//! the parser without spawning a process. Sub-commands that hit IPC, the FS,
//! or the daemon socket dispatch elsewhere — this module is dependency-free.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser, PartialEq)]
#[command(
    name = "clhorde-scheduler",
    about = "Workflow scheduler for spec-driven AI development",
    version
)]
pub struct Cli {
    /// Override the default log level (e.g. `debug`, `info,clhorde_scheduler=debug`).
    /// Falls back to `RUST_LOG` then `info`.
    #[arg(long, global = true)]
    pub log: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand, PartialEq)]
pub enum Command {
    /// Run as a long-lived watcher: connect to the daemon, subscribe to
    /// events, and pick up workflows as their `.clhorde-ready` markers
    /// appear. Stays foregrounded until SIGINT.
    Daemon(DaemonArgs),

    /// One-shot: pick up a queued change immediately, even without the
    /// long-lived daemon running.
    Apply(ApplyArgs),

    /// Run `/opsx:archive` for a verified change.
    Archive(NameArg),

    /// Cancel a running workflow.
    Cancel(NameArg),

    /// List unqueued OpenSpec changes ready to be promoted.
    Drafts(DraftsArgs),

    /// Spawn one PTY prompt running `/opsx:propose <idea>` and wait for a
    /// new `openspec/changes/<X>/` directory to appear.
    Propose(ProposeArgs),

    /// Write `openspec/changes/<name>/.clhorde-ready` so the scheduler picks
    /// the change up.
    Queue(QueueArgs),

    /// Re-dispatch a failed section.
    Retry(RetryArgs),

    /// Show workflow state(s).
    Status(StatusArgs),

    /// Manage prompt templates.
    Templates(TemplatesArgs),

    /// Remove `openspec/changes/<name>/.clhorde-ready`.
    Unqueue(NameArg),
}

#[derive(Debug, clap::Args, PartialEq)]
pub struct DaemonArgs {
    /// Watch this path instead of `$PWD` for `openspec/changes/`.
    #[arg(long)]
    pub root: Option<std::path::PathBuf>,
}

#[derive(Debug, clap::Args, PartialEq)]
pub struct ApplyArgs {
    /// OpenSpec change name (the directory under `openspec/changes/`).
    pub name: String,
    #[arg(long)]
    pub root: Option<std::path::PathBuf>,
}

#[derive(Debug, clap::Args, PartialEq)]
pub struct NameArg {
    pub name: String,
}

#[derive(Debug, clap::Args, PartialEq)]
pub struct DraftsArgs {
    #[arg(long)]
    pub root: Option<std::path::PathBuf>,
}

#[derive(Debug, clap::Args, PartialEq)]
pub struct ProposeArgs {
    /// Free-form description passed to `/opsx:propose`.
    pub idea: Vec<String>,
    #[arg(long)]
    pub root: Option<std::path::PathBuf>,
}

#[derive(Debug, clap::Args, PartialEq)]
pub struct QueueArgs {
    pub name: String,
    /// Higher runs first when workers free up.
    #[arg(long)]
    pub priority: Option<i32>,
    #[arg(long)]
    pub root: Option<std::path::PathBuf>,
}

#[derive(Debug, clap::Args, PartialEq)]
pub struct RetryArgs {
    pub name: String,
    /// Section number (e.g. `3`) or dotted task id (e.g. `3.2`).
    #[arg(long)]
    pub section: Option<String>,
}

#[derive(Debug, clap::Args, PartialEq)]
pub struct StatusArgs {
    /// If omitted, show every active and recent workflow.
    pub name: Option<String>,
}

#[derive(Debug, clap::Args, PartialEq)]
pub struct TemplatesArgs {
    #[command(subcommand)]
    pub action: TemplatesAction,
}

#[derive(Debug, Subcommand, PartialEq)]
pub enum TemplatesAction {
    /// Print the templates directory and exit.
    Path,
    /// Open the templates directory in `$EDITOR`.
    Edit,
}

impl Cli {
    /// Parse from an argv-style slice. Convenience for tests.
    pub fn try_parse_args<I, T>(args: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        Cli::try_parse_from(args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_args(args).expect("parse")
    }

    #[test]
    fn parses_daemon_subcommand() {
        let cli = parse(&["clhorde-scheduler", "daemon"]);
        assert!(matches!(cli.command, Command::Daemon(DaemonArgs { root: None })));
    }

    #[test]
    fn daemon_root_flag() {
        let cli = parse(&["clhorde-scheduler", "daemon", "--root", "/tmp/repo"]);
        match cli.command {
            Command::Daemon(DaemonArgs { root }) => {
                assert_eq!(root.unwrap().to_str(), Some("/tmp/repo"));
            }
            _ => panic!("expected Daemon"),
        }
    }

    #[test]
    fn parses_queue_with_priority() {
        let cli = parse(&[
            "clhorde-scheduler",
            "queue",
            "add-oauth",
            "--priority",
            "5",
        ]);
        match cli.command {
            Command::Queue(args) => {
                assert_eq!(args.name, "add-oauth");
                assert_eq!(args.priority, Some(5));
            }
            _ => panic!("expected Queue"),
        }
    }

    #[test]
    fn parses_unqueue() {
        let cli = parse(&["clhorde-scheduler", "unqueue", "add-oauth"]);
        assert_eq!(
            cli.command,
            Command::Unqueue(NameArg {
                name: "add-oauth".into()
            })
        );
    }

    #[test]
    fn status_optional_name() {
        let cli = parse(&["clhorde-scheduler", "status"]);
        assert_eq!(cli.command, Command::Status(StatusArgs { name: None }));

        let cli = parse(&["clhorde-scheduler", "status", "add-oauth"]);
        match cli.command {
            Command::Status(args) => assert_eq!(args.name.as_deref(), Some("add-oauth")),
            _ => panic!("expected Status"),
        }
    }

    #[test]
    fn retry_with_section_flag() {
        let cli = parse(&[
            "clhorde-scheduler",
            "retry",
            "add-oauth",
            "--section",
            "3.2",
        ]);
        match cli.command {
            Command::Retry(args) => {
                assert_eq!(args.name, "add-oauth");
                assert_eq!(args.section.as_deref(), Some("3.2"));
            }
            _ => panic!("expected Retry"),
        }
    }

    #[test]
    fn propose_collects_words_into_vec() {
        let cli = parse(&[
            "clhorde-scheduler",
            "propose",
            "Add",
            "OAuth",
            "login",
        ]);
        match cli.command {
            Command::Propose(args) => assert_eq!(args.idea, vec!["Add", "OAuth", "login"]),
            _ => panic!("expected Propose"),
        }
    }

    #[test]
    fn templates_path_subcommand() {
        let cli = parse(&["clhorde-scheduler", "templates", "path"]);
        match cli.command {
            Command::Templates(args) => assert_eq!(args.action, TemplatesAction::Path),
            _ => panic!("expected Templates"),
        }
    }

    #[test]
    fn templates_edit_subcommand() {
        let cli = parse(&["clhorde-scheduler", "templates", "edit"]);
        match cli.command {
            Command::Templates(args) => assert_eq!(args.action, TemplatesAction::Edit),
            _ => panic!("expected Templates"),
        }
    }

    #[test]
    fn rejects_unknown_subcommand() {
        let err = Cli::try_parse_args(["clhorde-scheduler", "nonsense"]).unwrap_err();
        assert!(matches!(
            err.kind(),
            clap::error::ErrorKind::InvalidSubcommand | clap::error::ErrorKind::UnknownArgument
        ));
    }

    #[test]
    fn missing_required_name_is_error() {
        let err = Cli::try_parse_args(["clhorde-scheduler", "queue"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn log_flag_is_global() {
        let cli = parse(&["clhorde-scheduler", "--log", "debug", "status"]);
        assert_eq!(cli.log.as_deref(), Some("debug"));
    }
}
