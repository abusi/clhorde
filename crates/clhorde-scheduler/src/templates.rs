//! Tera-backed template engine for prompt generation.
//!
//! Each phase of a workflow renders one template:
//!
//! | Template name    | Phase                        |
//! |------------------|------------------------------|
//! | `propose`        | `clhorde-scheduler propose`  |
//! | `apply-section`  | one prompt per DAG node      |
//! | `verify`         | end of apply phase           |
//! | `archive`        | end of verify phase          |
//!
//! Resolution order, per template name (highest priority first):
//! 1. **Per-project**: `<root>/openspec/.clhorde-scheduler/templates/<name>.md`
//! 2. **User**:        `~/.config/clhorde/scheduler/templates/<name>.md`
//! 3. **Built-in**:    compiled into the binary via `include_str!`
//!
//! The engine pre-loads every layer once at construction; callers never hit
//! the filesystem on a render call. Missing/invalid override files fall
//! through to the next layer with a warning — a typo in a user template
//! must never silently brick the scheduler.

use std::fs;
use std::path::{Path, PathBuf};

use tera::{Context, Tera};

/// Built-in template names. Each name maps 1:1 to a `*.md` file in this
/// crate's `templates/` directory, included at compile time.
pub const PROPOSE: &str = "propose";
pub const APPLY_SECTION: &str = "apply-section";
pub const VERIFY: &str = "verify";
pub const ARCHIVE: &str = "archive";

const BUILTIN_PROPOSE: &str = include_str!("../templates/propose.md");
const BUILTIN_APPLY_SECTION: &str = include_str!("../templates/apply-section.md");
const BUILTIN_VERIFY: &str = include_str!("../templates/verify.md");
const BUILTIN_ARCHIVE: &str = include_str!("../templates/archive.md");

const TEMPLATE_NAMES: &[&str] = &[PROPOSE, APPLY_SECTION, VERIFY, ARCHIVE];

#[derive(Debug)]
pub enum TemplateError {
    /// The requested template name is not one this engine recognises.
    UnknownTemplate(String),
    /// Render failure (missing variable, syntax error, etc.).
    Render(tera::Error),
    /// A template file existed but failed to compile when it was loaded.
    /// The engine logs and falls through, so this is rarely surfaced — only
    /// when *every* layer for a name has an error.
    Compile(tera::Error),
}

impl std::fmt::Display for TemplateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TemplateError::UnknownTemplate(n) => write!(f, "unknown template: {n}"),
            TemplateError::Render(e) => write!(f, "render: {e}"),
            TemplateError::Compile(e) => write!(f, "compile: {e}"),
        }
    }
}

impl std::error::Error for TemplateError {}

/// Layered template engine. Cheap to clone — `Tera` is internally arc'd.
#[derive(Debug, Clone)]
pub struct TemplateEngine {
    tera: Tera,
}

impl TemplateEngine {
    /// Build the engine with the standard layer order and the project root
    /// pointing at `<root>/openspec/.clhorde-scheduler/templates/`.
    pub fn new(root: &Path) -> Self {
        let user_dir = user_templates_dir();
        let project_dir = project_templates_dir(root);
        Self::from_dirs(user_dir.as_deref(), Some(project_dir.as_path()))
    }

    /// Test-friendly constructor that takes the user/project directories
    /// directly so `~/.config` doesn't sneak into unit tests.
    pub fn from_dirs(user: Option<&Path>, project: Option<&Path>) -> Self {
        let mut tera = Tera::default();
        // Disable autoescape — we render plain prompt text, not HTML.
        tera.autoescape_on(Vec::new());

        // Order matters: register built-ins first, then layer overrides on
        // top so they replace the entries by the same name.
        for (name, body) in builtins() {
            if let Err(e) = tera.add_raw_template(name, body) {
                // Built-ins are baked at compile time — a failure here is
                // a bug, surface loudly even outside test mode.
                tracing::error!(name, error = %e, "built-in template failed to compile");
            }
        }

        if let Some(dir) = user {
            load_dir_into(&mut tera, dir, "user");
        }
        if let Some(dir) = project {
            load_dir_into(&mut tera, dir, "project");
        }

        Self { tera }
    }

    /// Render a known template name with the given context. Returns
    /// `UnknownTemplate` if the name isn't one of the four built-ins (we
    /// reject it even if a user dropped a stray file with that name to
    /// keep the scheduler's surface area predictable).
    pub fn render(&self, name: &str, ctx: &Context) -> Result<String, TemplateError> {
        if !TEMPLATE_NAMES.contains(&name) {
            return Err(TemplateError::UnknownTemplate(name.to_string()));
        }
        self.tera.render(name, ctx).map_err(TemplateError::Render)
    }
}

/// `~/.config/clhorde/scheduler/templates/`
pub fn user_templates_dir() -> Option<PathBuf> {
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .ok()
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".config"))
        })?;
    Some(
        config_dir
            .join("clhorde")
            .join("scheduler")
            .join("templates"),
    )
}

/// `<root>/openspec/.clhorde-scheduler/templates/`
pub fn project_templates_dir(root: &Path) -> PathBuf {
    root.join("openspec")
        .join(".clhorde-scheduler")
        .join("templates")
}

fn builtins() -> [(&'static str, &'static str); 4] {
    [
        (PROPOSE, BUILTIN_PROPOSE),
        (APPLY_SECTION, BUILTIN_APPLY_SECTION),
        (VERIFY, BUILTIN_VERIFY),
        (ARCHIVE, BUILTIN_ARCHIVE),
    ]
}

fn load_dir_into(tera: &mut Tera, dir: &Path, layer_label: &str) {
    if !dir.is_dir() {
        return;
    }
    for &name in TEMPLATE_NAMES {
        // Accept both `<name>.md` and the bare `<name>` for forward-compat
        // with editors that may strip the extension.
        let candidates = [dir.join(format!("{name}.md")), dir.join(name)];
        for path in candidates {
            if !path.is_file() {
                continue;
            }
            match fs::read_to_string(&path) {
                Ok(body) => {
                    if let Err(e) = tera.add_raw_template(name, &body) {
                        tracing::warn!(
                            layer = layer_label,
                            template = name,
                            path = %path.display(),
                            error = %e,
                            "override template failed to compile; falling back"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        layer = layer_label,
                        template = name,
                        path = %path.display(),
                        error = %e,
                        "override template unreadable; falling back"
                    );
                }
            }
            break; // stop on first successful candidate
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn fixture_ctx() -> Context {
        let mut ctx = Context::new();
        ctx.insert("change_name", "add-oauth");
        ctx.insert(
            "change_dir",
            "/repo/openspec/changes/add-oauth",
        );
        ctx.insert("section_id", "1");
        ctx.insert("section_title", "Theme Infrastructure");
        ctx.insert("tasks_block", "- [ ] 1.1 First\n- [ ] 1.2 Second");
        ctx.insert("idea", "Add OAuth login");
        ctx
    }

    fn engine_no_overrides() -> TemplateEngine {
        TemplateEngine::from_dirs(None, None)
    }

    // ── built-in rendering ──

    #[test]
    fn renders_apply_section_with_fixture() {
        let out = engine_no_overrides()
            .render(APPLY_SECTION, &fixture_ctx())
            .unwrap();
        assert!(out.contains("OpenSpec change `add-oauth`"));
        assert!(out.contains("section 1 (Theme Infrastructure)"));
        assert!(out.contains("- [ ] 1.1 First"));
        assert!(out.contains("- [ ] 1.2 Second"));
        // Sanity: no leftover Tera markup.
        assert!(!out.contains("{{"));
    }

    #[test]
    fn renders_propose_template() {
        let out = engine_no_overrides()
            .render(PROPOSE, &fixture_ctx())
            .unwrap();
        assert!(out.contains("/opsx:propose Add OAuth login"));
    }

    #[test]
    fn renders_verify_template() {
        let out = engine_no_overrides()
            .render(VERIFY, &fixture_ctx())
            .unwrap();
        assert!(out.contains("OpenSpec change `add-oauth`"));
        assert!(out.contains("test suite"));
    }

    #[test]
    fn renders_archive_template() {
        let out = engine_no_overrides()
            .render(ARCHIVE, &fixture_ctx())
            .unwrap();
        assert!(out.contains("/opsx:archive add-oauth"));
    }

    #[test]
    fn unknown_template_name_is_an_error() {
        let err = engine_no_overrides()
            .render("not-a-template", &Context::new())
            .unwrap_err();
        match err {
            TemplateError::UnknownTemplate(n) => assert_eq!(n, "not-a-template"),
            other => panic!("expected UnknownTemplate, got {other}"),
        }
    }

    #[test]
    fn render_with_missing_variable_surfaces_error() {
        // apply-section needs `change_name` etc. — empty context fails.
        let err = engine_no_overrides()
            .render(APPLY_SECTION, &Context::new())
            .unwrap_err();
        assert!(matches!(err, TemplateError::Render(_)));
    }

    // ── override resolution ──

    #[test]
    fn user_override_replaces_builtin() {
        let tmp = TempDir::new().unwrap();
        let user_dir = tmp.path().to_path_buf();
        fs::write(
            user_dir.join(format!("{APPLY_SECTION}.md")),
            "USER override: {{ change_name }}",
        )
        .unwrap();

        let engine = TemplateEngine::from_dirs(Some(&user_dir), None);
        let out = engine.render(APPLY_SECTION, &fixture_ctx()).unwrap();
        assert_eq!(out, "USER override: add-oauth");
    }

    #[test]
    fn project_override_beats_user() {
        let user_tmp = TempDir::new().unwrap();
        let proj_tmp = TempDir::new().unwrap();
        fs::write(
            user_tmp.path().join(format!("{APPLY_SECTION}.md")),
            "USER",
        )
        .unwrap();
        fs::write(
            proj_tmp.path().join(format!("{APPLY_SECTION}.md")),
            "PROJECT",
        )
        .unwrap();

        let engine =
            TemplateEngine::from_dirs(Some(user_tmp.path()), Some(proj_tmp.path()));
        let out = engine.render(APPLY_SECTION, &fixture_ctx()).unwrap();
        assert_eq!(out, "PROJECT");
    }

    #[test]
    fn override_only_affects_named_template() {
        // Place a `verify.md` override but ask for `archive` — archive should
        // still come from the built-in.
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(format!("{VERIFY}.md")), "USER VERIFY").unwrap();

        let engine = TemplateEngine::from_dirs(Some(tmp.path()), None);

        let verify_out = engine.render(VERIFY, &fixture_ctx()).unwrap();
        assert_eq!(verify_out, "USER VERIFY");

        let archive_out = engine.render(ARCHIVE, &fixture_ctx()).unwrap();
        assert!(archive_out.contains("/opsx:archive add-oauth"));
    }

    #[test]
    fn missing_override_dir_uses_builtin() {
        let engine = TemplateEngine::from_dirs(
            Some(Path::new("/no/such/path")),
            Some(Path::new("/also/no/such/path")),
        );
        let out = engine.render(VERIFY, &fixture_ctx()).unwrap();
        assert!(out.contains("test suite"));
    }

    #[test]
    fn malformed_override_falls_through_to_builtin() {
        let tmp = TempDir::new().unwrap();
        // Tera will reject this — unbalanced delimiters.
        fs::write(tmp.path().join(format!("{ARCHIVE}.md")), "{{ no_end").unwrap();

        let engine = TemplateEngine::from_dirs(Some(tmp.path()), None);
        let out = engine.render(ARCHIVE, &fixture_ctx()).unwrap();
        // Built-in survived.
        assert!(out.contains("/opsx:archive add-oauth"));
    }

    #[test]
    fn override_without_extension_is_accepted() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join(VERIFY), "BARE NAME").unwrap();

        let engine = TemplateEngine::from_dirs(Some(tmp.path()), None);
        let out = engine.render(VERIFY, &fixture_ctx()).unwrap();
        assert_eq!(out, "BARE NAME");
    }

    #[test]
    fn project_dir_helper_points_at_expected_path() {
        let root = Path::new("/repo");
        let dir = project_templates_dir(root);
        assert_eq!(
            dir,
            PathBuf::from("/repo/openspec/.clhorde-scheduler/templates")
        );
    }
}
