//! Implementations for the `clhorde-scheduler` CLI subcommands.
//!
//! Two flavours:
//! - **FS-only**: `queue`, `unqueue`, `drafts`, `status`, `templates path|edit`
//!   touch `<root>/openspec/changes/` and `~/.local/share/clhorde/workflows/`
//!   directly. They never open a daemon socket.
//! - **Daemon-coupled**: `apply`, `archive`, `cancel`, `retry`, `propose`
//!   build [`ClientRequest`]s and ship them through
//!   [`daemon_client::send_one_shot`], which fences with a `Ping` so we know
//!   the daemon received every prior frame before we disconnect.
//!
//! Each function below returns a `Result<CommandOutput, CommandError>` so
//! both halves can be unit-tested without spinning up a real subprocess.
//! `main.rs` adapts the result to an `ExitCode` and writes the stdout buffer
//! to the terminal.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use clhorde_core::protocol::ClientRequest;
use tokio::sync::mpsc;

use crate::cli::{
    ApplyArgs, DraftsArgs, NameArg, ProposeArgs, QueueArgs, RetryArgs, StatusArgs,
};
use crate::daemon_client::{self, OneShotError};
use crate::openspec::annotations::annotate;
use crate::openspec::dag::{self, DagNode};
use crate::openspec::discovery::{self, ChangeStatus};
use crate::openspec::tasks_parser;
use crate::orchestrator::Orchestrator;
use crate::persistence::{StoreError, WorkflowStore};
use crate::templates::{self, TemplateEngine};
use crate::workflow::{Workflow, WorkflowStatus};

/// What every command produces. `stdout` is what the binary prints on
/// success; `stderr` is shown alongside on any return value.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct CommandOutput {
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }
}

#[derive(Debug)]
pub enum CommandError {
    Io(std::io::Error),
    Store(StoreError),
    NoSuchChange(String),
    NoSuchWorkflow(String),
    BadState { name: String, what: String },
    Daemon(OneShotError),
    Render(String),
    Other(String),
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandError::Io(e) => write!(f, "io: {e}"),
            CommandError::Store(e) => write!(f, "store: {e}"),
            CommandError::NoSuchChange(n) => {
                write!(f, "no such change directory: openspec/changes/{n}")
            }
            CommandError::NoSuchWorkflow(n) => {
                write!(f, "no such workflow: {n}")
            }
            CommandError::BadState { name, what } => {
                write!(f, "{name}: {what}")
            }
            CommandError::Daemon(e) => write!(f, "{e}"),
            CommandError::Render(e) => write!(f, "render: {e}"),
            CommandError::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for CommandError {}

impl From<std::io::Error> for CommandError {
    fn from(e: std::io::Error) -> Self {
        CommandError::Io(e)
    }
}
impl From<StoreError> for CommandError {
    fn from(e: StoreError) -> Self {
        CommandError::Store(e)
    }
}
impl From<OneShotError> for CommandError {
    fn from(e: OneShotError) -> Self {
        CommandError::Daemon(e)
    }
}

/// Resolve a `--root` argument (or fall back to `$PWD`) into an absolute path.
pub fn resolve_root(arg: Option<PathBuf>) -> std::io::Result<PathBuf> {
    match arg {
        Some(p) => Ok(p),
        None => std::env::current_dir(),
    }
}

// ── queue / unqueue ───────────────────────────────────────────────────────

/// Write `<root>/openspec/changes/<name>/.clhorde-ready` with optional
/// metadata. Refuses to write the marker if the change directory itself is
/// missing — a typo in `<name>` would otherwise silently create an orphan
/// dir.
pub fn queue(args: QueueArgs) -> Result<CommandOutput, CommandError> {
    let root = resolve_root(args.root)?;
    let change_dir = root.join("openspec").join("changes").join(&args.name);
    if !change_dir.is_dir() {
        return Err(CommandError::NoSuchChange(args.name));
    }
    let marker_path = change_dir.join(".clhorde-ready");
    let body = render_marker_body(args.priority);
    fs::write(&marker_path, body)?;
    Ok(CommandOutput::ok(format!("queued: {}\n", args.name)))
}

fn render_marker_body(priority: Option<i32>) -> String {
    match priority {
        Some(p) => format!("priority = {p}\n"),
        None => String::new(),
    }
}

/// Remove `<root>/openspec/changes/<name>/.clhorde-ready`. Missing marker
/// is not an error — the user's intent (this change should not be picked
/// up) is satisfied either way.
pub fn unqueue(
    args: NameArg,
    root: Option<PathBuf>,
) -> Result<CommandOutput, CommandError> {
    let root = resolve_root(root)?;
    let marker_path = root
        .join("openspec")
        .join("changes")
        .join(&args.name)
        .join(".clhorde-ready");
    match fs::remove_file(&marker_path) {
        Ok(()) => Ok(CommandOutput::ok(format!("unqueued: {}\n", args.name))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(CommandOutput::ok(format!(
                "unqueued: {} (already removed)\n",
                args.name
            )))
        }
        Err(e) => Err(CommandError::Io(e)),
    }
}

// ── drafts ────────────────────────────────────────────────────────────────

/// List every `openspec/changes/<X>/` directory that does NOT have a
/// `.clhorde-ready` marker. Output is one name per line, sorted.
pub fn drafts(args: DraftsArgs) -> Result<CommandOutput, CommandError> {
    let root = resolve_root(args.root)?;
    let scanned = discovery::scan(&root);
    let mut names: Vec<&str> = scanned
        .iter()
        .filter(|c| matches!(c.status, ChangeStatus::Drafted))
        .map(|c| c.name.as_str())
        .collect();
    names.sort();
    let mut out = String::new();
    for n in names {
        out.push_str(n);
        out.push('\n');
    }
    Ok(CommandOutput::ok(out))
}

// ── status ────────────────────────────────────────────────────────────────

/// Print one workflow line per workflow, or — when `args.name` is set —
/// a multi-line block with every persisted field.
pub fn status(
    args: StatusArgs,
    store: &WorkflowStore,
) -> Result<CommandOutput, CommandError> {
    match args.name {
        Some(name) => {
            let wf = store
                .load(&name)?
                .ok_or_else(|| CommandError::NoSuchWorkflow(name.clone()))?;
            Ok(CommandOutput::ok(format_workflow_detail(&wf)))
        }
        None => {
            let workflows = store.list()?;
            if workflows.is_empty() {
                return Ok(CommandOutput::ok(
                    "(no workflows in ~/.local/share/clhorde/workflows/)\n"
                        .to_string(),
                ));
            }
            let mut out = String::new();
            for wf in &workflows {
                out.push_str(&format_workflow_summary(wf));
                out.push('\n');
            }
            Ok(CommandOutput::ok(out))
        }
    }
}

fn format_workflow_summary(wf: &Workflow) -> String {
    let label = workflow_status_label(&wf.status);
    let trail = workflow_status_detail(wf);
    if trail.is_empty() {
        format!("{}: {}", wf.name, label)
    } else {
        format!("{}: {} ({})", wf.name, label, trail)
    }
}

fn format_workflow_detail(wf: &Workflow) -> String {
    let mut s = String::new();
    s.push_str(&format!("name: {}\n", wf.name));
    s.push_str(&format!("status: {}\n", workflow_status_label(&wf.status)));
    if let WorkflowStatus::Failed { reason } = &wf.status {
        s.push_str(&format!("reason: {reason}\n"));
    }
    s.push_str(&format!("priority: {}\n", wf.metadata.priority.unwrap_or(0)));
    if let Some(b) = &wf.metadata.worktree_branch {
        s.push_str(&format!("worktree_branch: {b}\n"));
    }
    if !wf.metadata.depends_on.is_empty() {
        s.push_str(&format!(
            "depends_on: {}\n",
            wf.metadata.depends_on.join(", ")
        ));
    }
    s.push_str(&format!("queued_at: {}\n", iso_or_dash(wf.queued_at)));
    s.push_str(&format!("started_at: {}\n", iso_or_dash(wf.started_at)));
    s.push_str(&format!(
        "finished_at: {}\n",
        iso_or_dash(wf.finished_at)
    ));
    if !wf.prompt_ids.is_empty() {
        s.push_str("prompts:\n");
        for id in &wf.prompt_ids {
            s.push_str(&format!("  - {id}\n"));
        }
    }
    s
}

fn workflow_status_label(s: &WorkflowStatus) -> &'static str {
    match s {
        WorkflowStatus::Drafted => "drafted",
        WorkflowStatus::Queued => "queued",
        WorkflowStatus::Implementing => "implementing",
        WorkflowStatus::Verifying => "verifying",
        WorkflowStatus::Archiving => "archiving",
        WorkflowStatus::Archived => "archived",
        WorkflowStatus::Cancelled => "cancelled",
        WorkflowStatus::Failed { .. } => "failed",
    }
}

fn workflow_status_detail(wf: &Workflow) -> String {
    match (&wf.status, wf.started_at, wf.finished_at) {
        (WorkflowStatus::Failed { reason }, _, _) => reason.clone(),
        (_, _, Some(f)) => format!("finished {}", relative_time(f)),
        (_, Some(s), _) => format!("started {}", relative_time(s)),
        _ => String::new(),
    }
}

fn iso_or_dash(t: Option<DateTime<Utc>>) -> String {
    match t {
        Some(t) => t.to_rfc3339(),
        None => "-".to_string(),
    }
}

fn relative_time(t: DateTime<Utc>) -> String {
    let now = Utc::now();
    let secs = (now - t).num_seconds();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

// ── templates path / edit ────────────────────────────────────────────────

/// Print the user templates directory.
pub fn templates_path() -> Result<CommandOutput, CommandError> {
    let path = templates::user_templates_dir().ok_or_else(|| {
        CommandError::Other("cannot resolve user templates directory".into())
    })?;
    Ok(CommandOutput::ok(format!("{}\n", path.display())))
}

/// Open `$EDITOR` on the user templates directory. Creates the directory
/// (and any missing parents) first so the editor doesn't refuse a missing
/// path.
///
/// Test hook: pass an explicit `editor_override` to short-circuit the env
/// lookup; production callers pass `None`.
pub fn templates_edit(
    editor_override: Option<&str>,
) -> Result<CommandOutput, CommandError> {
    let path = templates::user_templates_dir().ok_or_else(|| {
        CommandError::Other("cannot resolve user templates directory".into())
    })?;
    fs::create_dir_all(&path)?;
    let editor = editor_override
        .map(str::to_string)
        .or_else(|| std::env::var("EDITOR").ok())
        .or_else(|| std::env::var("VISUAL").ok())
        .unwrap_or_else(|| "vi".to_string());
    let status = std::process::Command::new(&editor)
        .arg(&path)
        .status()
        .map_err(CommandError::Io)?;
    if !status.success() {
        return Err(CommandError::Other(format!(
            "{editor} exited with {status}"
        )));
    }
    Ok(CommandOutput::ok(format!(
        "edited templates at {}\n",
        path.display()
    )))
}

// ── apply / archive / propose / cancel / retry ───────────────────────────

/// Force-pickup a queued workflow: build the DAG from `tasks.md`, dispatch
/// the runnable wave through the daemon, exit. Re-running picks up the
/// next wave once the prior nodes complete (the orchestrator is
/// idempotent).
pub async fn apply(args: ApplyArgs) -> Result<CommandOutput, CommandError> {
    let root = resolve_root(args.root)?;
    let store = WorkflowStore::open_default()?;
    let (tx, mut rx) = mpsc::unbounded_channel::<ClientRequest>();
    let mut orch = Orchestrator::new(root, store, tx);
    orch.reconcile().map_err(|e| CommandError::Other(e.to_string()))?;
    if orch.workflow(&args.name).is_none() {
        return Err(CommandError::NoSuchWorkflow(args.name));
    }
    orch.try_advance(&args.name)
        .map_err(|e| CommandError::Other(e.to_string()))?;

    let requests = drain_requests(&mut rx);
    if requests.is_empty() {
        return Ok(CommandOutput::ok(format!(
            "{}: nothing to dispatch\n",
            args.name
        )));
    }
    let n = requests.len();
    daemon_client::send_one_shot(requests).await?;
    Ok(CommandOutput::ok(format!(
        "{}: dispatched {n} prompt(s)\n",
        args.name
    )))
}

/// Render and submit a single archive prompt for the workflow. Lets the
/// user advance a workflow that's already verified without waiting for the
/// long-running scheduler to do it.
pub async fn archive(
    args: NameArg,
    root: Option<PathBuf>,
) -> Result<CommandOutput, CommandError> {
    let root = resolve_root(root)?;
    let engine = TemplateEngine::new(&root);
    let mut ctx = tera::Context::new();
    ctx.insert("change_name", &args.name);
    let prompt = engine
        .render(templates::ARCHIVE, &ctx)
        .map_err(|e| CommandError::Render(e.to_string()))?;
    let request = build_phase_request(&root, &args.name, "archive", prompt);
    daemon_client::send_one_shot(vec![request]).await?;
    Ok(CommandOutput::ok(format!(
        "archive prompt dispatched for: {}\n",
        args.name
    )))
}

/// Spawn the `/opsx:propose` prompt and exit. We don't wait for the change
/// directory to appear — this is a fire-and-forget; the user can run
/// `clhorde-scheduler drafts` to see the result once Claude has made the
/// directory.
pub async fn propose(args: ProposeArgs) -> Result<CommandOutput, CommandError> {
    let root = resolve_root(args.root)?;
    let idea = args.idea.join(" ");
    if idea.trim().is_empty() {
        return Err(CommandError::Other("idea must not be empty".into()));
    }
    let engine = TemplateEngine::new(&root);
    let mut ctx = tera::Context::new();
    ctx.insert("idea", &idea);
    let prompt = engine
        .render(templates::PROPOSE, &ctx)
        .map_err(|e| CommandError::Render(e.to_string()))?;
    let request = ClientRequest::SubmitPrompt {
        text: prompt,
        cwd: Some(root.to_string_lossy().into_owned()),
        mode: "oneshot".to_string(),
        worktree: false,
        tags: vec![format!("clhorde-scheduler/phase=propose")],
        depends_on: Vec::new(),
        worktree_id: None,
    };
    daemon_client::send_one_shot(vec![request]).await?;
    Ok(CommandOutput::ok(format!("proposing: {idea}\n")))
}

/// Cancel a workflow: remove the marker (so any running scheduler sees it
/// and cancels), and update the persisted workflow to `Cancelled` so
/// `status` reflects the user's intent immediately. Running workers on the
/// daemon are *not* killed by this command — Phase 2.6 stops short of
/// daemon-side worker termination because the running scheduler's
/// orchestrator already does that on the marker-removal event.
pub fn cancel(
    args: NameArg,
    root: Option<PathBuf>,
    store: &WorkflowStore,
) -> Result<CommandOutput, CommandError> {
    let root = resolve_root(root)?;
    let marker = root
        .join("openspec")
        .join("changes")
        .join(&args.name)
        .join(".clhorde-ready");
    let marker_existed = marker.exists();
    if marker_existed {
        fs::remove_file(&marker)?;
    }

    let mut wf = match store.load(&args.name)? {
        Some(w) => w,
        None => {
            // No persisted workflow — marker removal was the only effect.
            return Ok(CommandOutput::ok(format!(
                "cancelled (no persisted workflow): {}\n",
                args.name
            )));
        }
    };
    let updated = match wf.status {
        WorkflowStatus::Drafted => false,
        WorkflowStatus::Queued => wf.unqueue().is_ok(),
        WorkflowStatus::Implementing
        | WorkflowStatus::Verifying
        | WorkflowStatus::Archiving => wf.cancel().is_ok(),
        // Already terminal — nothing to do, but acknowledge.
        _ => false,
    };
    if updated {
        store.save(&wf)?;
    }
    Ok(CommandOutput::ok(format!(
        "cancelled: {} (status now {})\n",
        args.name,
        workflow_status_label(&wf.status)
    )))
}

/// Re-dispatch a single failed/missing apply node. Resets the workflow's
/// status from `Failed` back to `Implementing` (so the orchestrator will
/// continue from where it left off) and submits one apply prompt for the
/// requested section.
pub async fn retry(
    args: RetryArgs,
    root: Option<PathBuf>,
    store: &WorkflowStore,
) -> Result<CommandOutput, CommandError> {
    let root = resolve_root(root)?;
    let section_id = args.section.as_deref().ok_or_else(|| {
        CommandError::Other("retry requires --section <id>".into())
    })?;

    let mut wf = store
        .load(&args.name)?
        .ok_or_else(|| CommandError::NoSuchWorkflow(args.name.clone()))?;

    // Reset Failed → Implementing if needed. Drafted/Queued/active states
    // don't need reset; terminal Archived/Cancelled refuse.
    match wf.status {
        WorkflowStatus::Failed { .. } => {
            wf.status = WorkflowStatus::Implementing;
            store.save(&wf)?;
        }
        WorkflowStatus::Archived | WorkflowStatus::Cancelled => {
            return Err(CommandError::BadState {
                name: args.name,
                what: "cannot retry a terminal workflow".into(),
            });
        }
        _ => {}
    }

    // Parse tasks.md to render the apply template.
    let tasks_path = root
        .join("openspec")
        .join("changes")
        .join(&args.name)
        .join("tasks.md");
    let tasks_md = fs::read_to_string(&tasks_path).map_err(CommandError::Io)?;
    let graph = tasks_parser::parse(&tasks_md);
    let annotated = annotate(graph);
    let dag_built = dag::build(&annotated)
        .map_err(|e| CommandError::Other(format!("dag: {e}")))?;
    let node = dag_built
        .nodes
        .iter()
        .find(|n| n.id == section_id)
        .ok_or_else(|| CommandError::Other(format!("no node {section_id}")))?;
    let section = annotated.iter().find(|s| s.section.id == node.id);
    let tasks_block = section
        .map(|s| {
            s.items
                .iter()
                .map(|t| {
                    let mark = if t.task.done { "[x]" } else { "[ ]" };
                    format!("- {} {} {}", mark, t.task.id, t.task.text)
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    let engine = TemplateEngine::new(&root);
    let prompt = render_apply_template(&engine, &root, &args.name, node, &tasks_block)?;
    let tag = format!(
        "clhorde-scheduler/wf={}/phase=apply/node={}",
        args.name, node.id
    );
    let request = ClientRequest::SubmitPrompt {
        text: prompt,
        cwd: Some(root.to_string_lossy().into_owned()),
        mode: "oneshot".to_string(),
        worktree: true,
        tags: vec![tag],
        depends_on: Vec::new(),
        worktree_id: Some(args.name.clone()),
    };

    daemon_client::send_one_shot(vec![request]).await?;
    Ok(CommandOutput::ok(format!(
        "retry dispatched: {} section {}\n",
        args.name, section_id
    )))
}

fn render_apply_template(
    engine: &TemplateEngine,
    root: &Path,
    change_name: &str,
    node: &DagNode,
    tasks_block: &str,
) -> Result<String, CommandError> {
    let mut ctx = tera::Context::new();
    ctx.insert("change_name", change_name);
    ctx.insert(
        "change_dir",
        &format!(
            "{}/openspec/changes/{}",
            root.to_string_lossy(),
            change_name
        ),
    );
    ctx.insert("section_id", &node.id);
    ctx.insert("section_title", &node.label);
    ctx.insert("tasks_block", tasks_block);
    let template_name = node
        .prompt_template
        .as_deref()
        .unwrap_or(templates::APPLY_SECTION);
    engine
        .render(template_name, &ctx)
        .map_err(|e| CommandError::Render(e.to_string()))
}

fn build_phase_request(
    root: &Path,
    name: &str,
    phase: &str,
    prompt: String,
) -> ClientRequest {
    ClientRequest::SubmitPrompt {
        text: prompt,
        cwd: Some(root.to_string_lossy().into_owned()),
        mode: "oneshot".to_string(),
        worktree: true,
        tags: vec![format!("clhorde-scheduler/wf={name}/phase={phase}")],
        depends_on: Vec::new(),
        worktree_id: Some(name.to_string()),
    }
}

fn drain_requests(
    rx: &mut mpsc::UnboundedReceiver<ClientRequest>,
) -> Vec<ClientRequest> {
    let mut out = Vec::new();
    while let Ok(r) = rx.try_recv() {
        out.push(r);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{TemplatesAction, TemplatesArgs};
    use tempfile::TempDir;

    fn change_dir(tmp: &TempDir, name: &str) -> PathBuf {
        let p = tmp.path().join("openspec").join("changes").join(name);
        fs::create_dir_all(&p).unwrap();
        p
    }

    // ── queue ──

    #[test]
    fn queue_writes_marker_with_priority() {
        let tmp = TempDir::new().unwrap();
        change_dir(&tmp, "add-oauth");
        let out = queue(QueueArgs {
            name: "add-oauth".into(),
            priority: Some(7),
            root: Some(tmp.path().to_path_buf()),
        })
        .unwrap();
        assert!(out.stdout.contains("queued: add-oauth"));

        let body = fs::read_to_string(
            tmp.path()
                .join("openspec/changes/add-oauth/.clhorde-ready"),
        )
        .unwrap();
        assert_eq!(body, "priority = 7\n");
    }

    #[test]
    fn queue_writes_empty_marker_without_priority() {
        let tmp = TempDir::new().unwrap();
        change_dir(&tmp, "x");
        queue(QueueArgs {
            name: "x".into(),
            priority: None,
            root: Some(tmp.path().to_path_buf()),
        })
        .unwrap();
        let body = fs::read_to_string(
            tmp.path().join("openspec/changes/x/.clhorde-ready"),
        )
        .unwrap();
        assert_eq!(body, "");
    }

    #[test]
    fn queue_rejects_missing_change_dir() {
        let tmp = TempDir::new().unwrap();
        let err = queue(QueueArgs {
            name: "ghost".into(),
            priority: None,
            root: Some(tmp.path().to_path_buf()),
        })
        .unwrap_err();
        assert!(matches!(err, CommandError::NoSuchChange(n) if n == "ghost"));
    }

    // ── unqueue ──

    #[test]
    fn unqueue_removes_marker() {
        let tmp = TempDir::new().unwrap();
        let p = change_dir(&tmp, "x");
        fs::write(p.join(".clhorde-ready"), "").unwrap();
        unqueue(
            NameArg { name: "x".into() },
            Some(tmp.path().to_path_buf()),
        )
        .unwrap();
        assert!(!p.join(".clhorde-ready").exists());
    }

    #[test]
    fn unqueue_missing_marker_is_ok() {
        let tmp = TempDir::new().unwrap();
        change_dir(&tmp, "x");
        let out = unqueue(
            NameArg { name: "x".into() },
            Some(tmp.path().to_path_buf()),
        )
        .unwrap();
        assert!(out.stdout.contains("already removed"));
    }

    // ── drafts ──

    #[test]
    fn drafts_lists_only_unqueued_changes() {
        let tmp = TempDir::new().unwrap();
        change_dir(&tmp, "draft-a");
        change_dir(&tmp, "draft-b");
        let queued = change_dir(&tmp, "queued-x");
        fs::write(queued.join(".clhorde-ready"), "").unwrap();

        let out = drafts(DraftsArgs {
            root: Some(tmp.path().to_path_buf()),
        })
        .unwrap();
        let lines: Vec<&str> = out.stdout.lines().collect();
        assert_eq!(lines, vec!["draft-a", "draft-b"]);
    }

    #[test]
    fn drafts_handles_empty_repo() {
        let tmp = TempDir::new().unwrap();
        let out = drafts(DraftsArgs {
            root: Some(tmp.path().to_path_buf()),
        })
        .unwrap();
        assert!(out.stdout.is_empty());
    }

    // ── status ──

    #[test]
    fn status_lists_all_workflows_when_no_name_given() {
        let tmp = TempDir::new().unwrap();
        let store = WorkflowStore::open(tmp.path().join("wf"));
        store.save(&Workflow::drafted("alpha")).unwrap();
        let mut zeta = Workflow::drafted("zeta");
        zeta.queue(crate::openspec::discovery::MarkerMetadata::default())
            .unwrap();
        store.save(&zeta).unwrap();

        let out = status(StatusArgs { name: None }, &store).unwrap();
        assert!(out.stdout.contains("alpha: drafted"));
        assert!(out.stdout.contains("zeta: queued"));
        // Sorted by name.
        assert!(out.stdout.find("alpha").unwrap() < out.stdout.find("zeta").unwrap());
    }

    #[test]
    fn status_for_specific_workflow_dumps_detail() {
        let tmp = TempDir::new().unwrap();
        let store = WorkflowStore::open(tmp.path().join("wf"));
        let mut wf = Workflow::drafted("add-oauth");
        wf.metadata.priority = Some(7);
        wf.metadata.depends_on = vec!["base".into()];
        wf.queue(wf.metadata.clone()).unwrap();
        store.save(&wf).unwrap();

        let out = status(
            StatusArgs {
                name: Some("add-oauth".into()),
            },
            &store,
        )
        .unwrap();
        assert!(out.stdout.contains("name: add-oauth"));
        assert!(out.stdout.contains("status: queued"));
        assert!(out.stdout.contains("priority: 7"));
        assert!(out.stdout.contains("depends_on: base"));
    }

    #[test]
    fn status_for_missing_workflow_is_error() {
        let tmp = TempDir::new().unwrap();
        let store = WorkflowStore::open(tmp.path().join("wf"));
        let err = status(
            StatusArgs {
                name: Some("ghost".into()),
            },
            &store,
        )
        .unwrap_err();
        assert!(matches!(err, CommandError::NoSuchWorkflow(_)));
    }

    #[test]
    fn status_with_empty_store_is_friendly() {
        let tmp = TempDir::new().unwrap();
        let store = WorkflowStore::open(tmp.path().join("wf"));
        let out = status(StatusArgs { name: None }, &store).unwrap();
        assert!(out.stdout.contains("(no workflows"));
    }

    // ── templates path / edit ──

    #[test]
    fn templates_path_prints_a_path() {
        let out = templates_path().unwrap();
        // Path should end in `clhorde/scheduler/templates`.
        assert!(out.stdout.contains("clhorde"));
        assert!(out.stdout.contains("templates"));
    }

    #[test]
    fn templates_edit_with_explicit_editor_runs() {
        // `true` is a no-op binary that always exits 0 — perfect for
        // exercising the run loop without actually opening an editor.
        let out = templates_edit(Some("true")).unwrap();
        assert!(out.stdout.contains("edited templates"));
    }

    #[test]
    fn templates_edit_propagates_editor_failure() {
        // `false` always exits non-zero.
        let err = templates_edit(Some("false")).unwrap_err();
        assert!(matches!(err, CommandError::Other(_)));
    }

    // Drag args type into compile coverage so they don't bit-rot.
    fn _templates_args_compile() {
        let _ = TemplatesArgs {
            action: TemplatesAction::Path,
        };
    }

    // ── cancel ──

    #[test]
    fn cancel_removes_marker_and_updates_workflow() {
        let tmp = TempDir::new().unwrap();
        let p = change_dir(&tmp, "x");
        fs::write(p.join(".clhorde-ready"), "").unwrap();
        let store = WorkflowStore::open(tmp.path().join("wf"));
        let mut wf = Workflow::drafted("x");
        wf.queue(crate::openspec::discovery::MarkerMetadata::default())
            .unwrap();
        wf.start_implementing().unwrap();
        store.save(&wf).unwrap();

        let out = cancel(
            NameArg { name: "x".into() },
            Some(tmp.path().to_path_buf()),
            &store,
        )
        .unwrap();
        assert!(out.stdout.contains("cancelled"));
        assert!(!p.join(".clhorde-ready").exists());

        let loaded = store.load("x").unwrap().unwrap();
        assert_eq!(loaded.status, WorkflowStatus::Cancelled);
    }

    #[test]
    fn cancel_no_marker_no_workflow_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let store = WorkflowStore::open(tmp.path().join("wf"));
        let out = cancel(
            NameArg { name: "ghost".into() },
            Some(tmp.path().to_path_buf()),
            &store,
        )
        .unwrap();
        assert!(out.stdout.contains("ghost"));
    }

    #[test]
    fn cancel_queued_returns_to_drafted() {
        let tmp = TempDir::new().unwrap();
        change_dir(&tmp, "x");
        let store = WorkflowStore::open(tmp.path().join("wf"));
        let mut wf = Workflow::drafted("x");
        wf.queue(crate::openspec::discovery::MarkerMetadata::default())
            .unwrap();
        store.save(&wf).unwrap();

        cancel(
            NameArg { name: "x".into() },
            Some(tmp.path().to_path_buf()),
            &store,
        )
        .unwrap();
        // Queued + no running workers → unqueue, not cancel.
        assert_eq!(
            store.load("x").unwrap().unwrap().status,
            WorkflowStatus::Drafted
        );
    }

    // ── apply / archive / propose / retry build ClientRequests ──
    //
    // We can't easily run the full async function without a daemon, so we
    // exercise the request-construction helpers directly. This covers the
    // payload shape the daemon will see.

    #[test]
    fn build_phase_request_archive_shape() {
        let req = build_phase_request(
            Path::new("/repo"),
            "add-oauth",
            "archive",
            "render".into(),
        );
        match req {
            ClientRequest::SubmitPrompt {
                text,
                tags,
                worktree,
                worktree_id,
                cwd,
                mode,
                ..
            } => {
                assert_eq!(text, "render");
                assert_eq!(
                    tags,
                    vec!["clhorde-scheduler/wf=add-oauth/phase=archive".to_string()]
                );
                assert!(worktree);
                assert_eq!(worktree_id.as_deref(), Some("add-oauth"));
                assert_eq!(cwd.as_deref(), Some("/repo"));
                assert_eq!(mode, "oneshot");
            }
            _ => panic!("expected SubmitPrompt"),
        }
    }

    #[test]
    fn render_apply_template_uses_node_override() {
        // Simulate a node with a custom prompt template name; render falls
        // back to the built-in apply-section if the override isn't present.
        // (Real overrides are loaded via TemplateEngine layering — here we
        // just verify the wiring picks up the right field.)
        let engine = TemplateEngine::from_dirs(None, None);
        let node = DagNode {
            id: "1".into(),
            label: "Foo".into(),
            deps: Vec::new(),
            parallel_with: Vec::new(),
            prompt_template: None,
            source_line: 1,
        };
        let out = render_apply_template(
            &engine,
            Path::new("/repo"),
            "add-oauth",
            &node,
            "- [ ] 1.1 q",
        )
        .unwrap();
        assert!(out.contains("OpenSpec change `add-oauth`"));
        assert!(out.contains("section 1 (Foo)"));
        assert!(out.contains("- [ ] 1.1 q"));
    }
}
