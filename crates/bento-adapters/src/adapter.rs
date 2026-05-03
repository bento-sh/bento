//! The [`LanguageAdapter`] trait and its supporting value types.

use std::path::{Path, PathBuf};

use anyhow::Result;

/// A specific toolchain version required by a dish (e.g. `go 1.22.3`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolVersion {
    /// Canonical tool name (`go`, `node`, `bun`, `deno`, `rust`).
    pub tool: String,
    /// Version as declared by the project (`1.22`, `1.22.3`, `22.1.0`, ...).
    pub version: String,
}

/// A default task recipe supplied by an adapter. A dish's own `[tasks.<name>]`
/// block always wins over defaults of the same name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultTask {
    pub name: String,
    pub run: String,
    pub inputs: Option<Vec<String>>,
    pub outputs: Option<Vec<String>>,
}

/// A task the adapter detected from project metadata at *init time* —
/// e.g. a script in `package.json` or `composer.json`. Surfaced by
/// [`LanguageAdapter::detected_tasks`] so `bento init` can pre-populate
/// `[tasks.<name>]` blocks in the generated `dish.toml`.
///
/// Distinct from [`DefaultTask`]: defaults are baked-in adapter
/// recipes that apply universally (`build`/`test`/`lint`); detected
/// tasks reflect what *this specific project* actually wires up. CI
/// flows want every script mirrored, not just the standard names —
/// monorepos lean on bento for everything except deploy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedTask {
    pub name: String,
    pub run: String,
}

/// Runtime context for a task invocation (install, build, test, ...).
#[derive(Debug, Clone)]
pub struct TaskContext {
    /// Directory containing `dish.toml` (and, typically, the source).
    pub dish_dir: PathBuf,
    /// `dish.name` — useful for log prefixes and error messages.
    pub dish_name: String,
    /// PATH entries to prepend for child-process execution. Populated
    /// by the executor when a toolchain pin is in effect (so `npm`,
    /// `composer`, etc. resolve to the pinned install); empty when
    /// the dish inherits from the host PATH.
    pub toolchain_paths: Vec<PathBuf>,
}

/// Options for [`LanguageAdapter::add`]. Currently just the dev /
/// runtime split — adapters that don't have a dev-deps concept (`go
/// get`, …) ignore the flag with a warning surfaced in `Added::note`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddOptions {
    /// Whether to add as a dev / build-time dependency. Maps to
    /// `cargo add --dev`, `bun add -d`, `npm install --save-dev`,
    /// `pnpm add -D`, `yarn add --dev`. Adapters without the concept
    /// (Go, Deno's runtime imports) emit a `note` and add as a
    /// regular dependency.
    pub dev: bool,
}

/// One package added by [`LanguageAdapter::add`]. `version` is the
/// resolved version string if the package manager reported it (e.g.
/// `cargo add` echoes `Adding serde v1.0.215`); `note` is a free-form
/// hint surfaced to the operator when something non-fatal happened
/// (e.g. `--dev` ignored on a Go dish).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Added {
    /// The package spec the adapter resolved — typically the input
    /// echoed back, sometimes the canonical form (`tailwindcss@3.4.0`
    /// → `tailwindcss`).
    pub package: String,
    pub version: Option<String>,
    pub note: Option<String>,
}

/// Result of a cheap "are dependencies installed?" probe. Used by the
/// executor to decide whether to run [`LanguageAdapter::install`] before
/// any tasks execute.
///
/// The probe is a file-existence check only — no hashing, no subprocess
/// calls — so paying its cost on every run is effectively free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallProbe {
    /// Deps look present; task runs can proceed without reinstalling.
    Ready,
    /// Deps are missing or torn. The executor should invoke
    /// [`LanguageAdapter::install`]. `reason` is a human-readable hint
    /// surfaced in logs (`"node_modules/.package-lock.json absent"`).
    Missing { reason: String },
}

impl InstallProbe {
    /// Convenience: construct a [`InstallProbe::Missing`].
    pub fn missing(reason: impl Into<String>) -> Self {
        InstallProbe::Missing {
            reason: reason.into(),
        }
    }
}

impl TaskContext {
    pub fn new(dish_dir: impl Into<PathBuf>, dish_name: impl Into<String>) -> Self {
        Self {
            dish_dir: dish_dir.into(),
            dish_name: dish_name.into(),
            toolchain_paths: Vec::new(),
        }
    }

    /// Attach toolchain PATH entries to this context (builder style).
    pub fn with_toolchain_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.toolchain_paths = paths;
        self
    }

    /// Apply `dish_dir` and any `toolchain_paths` PATH prepend to a
    /// [`std::process::Command`] before spawning. Adapters should call
    /// this from their [`LanguageAdapter::install`] implementations so
    /// pinned toolchains resolve correctly.
    pub fn apply_env(&self, cmd: &mut std::process::Command) {
        cmd.current_dir(&self.dish_dir);
        if !self.toolchain_paths.is_empty() {
            let mut entries = self.toolchain_paths.clone();
            if let Some(existing) = std::env::var_os("PATH") {
                for p in std::env::split_paths(&existing) {
                    entries.push(p);
                }
            }
            if let Ok(joined) = std::env::join_paths(entries) {
                cmd.env("PATH", joined);
            }
        }
    }
}

/// Run an install-style package-manager command with stdio captured
/// so it can't pollute bento's own stdout (which, under `--json`,
/// must stay parseable machine output). On success the captured
/// output is discarded silently — bento's ExecutionSummary already
/// reports install state via `installs` / `install_failures` plus
/// a per-dish `InstallRecord`. On failure the captured stdout +
/// stderr are attached to the returned error so the operator sees
/// why `npm ci` / `composer install` / `mvn package` etc. blew up.
///
/// `label` is a human-readable descriptor for error messages —
/// typically the verb being run (`"npm ci"`, `"bundle install"`, …).
pub fn run_install_cmd(
    ctx: &TaskContext,
    cmd: &mut std::process::Command,
    label: &str,
) -> Result<()> {
    use anyhow::Context;
    let out = cmd
        .output()
        .with_context(|| format!("invoking `{label}` in {}", ctx.dish_dir.display()))?;
    if !out.status.success() {
        let code = out.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        anyhow::bail!(
            "`{label}` exited {code}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            stdout.trim_end(),
            stderr.trim_end(),
        );
    }
    Ok(())
}

/// Run an `add`-style package-manager command and return its captured
/// stdout for parsing. Same surface as [`run_install_cmd`] (stderr +
/// non-zero exit attached to the error) but hands stdout back to the
/// caller because adapters need to parse "added X v1.2.3" lines out
/// of it. Stderr is captured and discarded on success — keeps bento's
/// own stdout parseable when the user passes `--json`.
pub fn run_add_cmd(
    ctx: &TaskContext,
    cmd: &mut std::process::Command,
    label: &str,
) -> Result<String> {
    use anyhow::Context;
    let out = cmd
        .output()
        .with_context(|| format!("invoking `{label}` in {}", ctx.dish_dir.display()))?;
    if !out.status.success() {
        let code = out.status.code().unwrap_or(-1);
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        anyhow::bail!(
            "`{label}` exited {code}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            stdout.trim_end(),
            stderr.trim_end(),
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// An adapter teaches bento about a specific language / package manager.
///
/// Implementations must be cheap to construct: the registry holds them behind
/// `Box<dyn LanguageAdapter>` and calls them repeatedly during a plan.
pub trait LanguageAdapter: Send + Sync {
    /// Stable identifier. Matches `dish.toml`'s `language` field
    /// (`"go"`, `"node-npm"`, `"bun"`, ...).
    ///
    /// Returns `&str` (not `&'static str`) so that out-of-process plugin
    /// adapters can borrow from a heap-allocated id field without leaking.
    /// Built-in literals coerce trivially.
    fn id(&self) -> &str;

    /// Human-readable name for display. Defaults to `id`.
    fn display_name(&self) -> &str {
        self.id()
    }

    /// Does `dir` look like a project of this language?
    ///
    /// Must be cheap (file existence checks, not content parsing) — called
    /// once per dish directory during workspace discovery.
    fn detect(&self, dir: &Path) -> bool;

    /// Files that, when changed, should invalidate this dish's cache.
    ///
    /// Paths are relative to the dish dir. Typical entries: lockfiles,
    /// toolchain pin files. Source files themselves are covered by each
    /// task's own `inputs` globs.
    ///
    /// Returns `Vec<String>` (owned) so plugin adapters can return the
    /// list out of their handshake-time manifest. Built-ins clone a
    /// small constant array. Called once per `(adapter, dish)` during
    /// planning — the allocation cost is dust.
    fn fingerprint_files(&self) -> Vec<String>;

    /// Globs for files this adapter generates into the dish dir as a
    /// side effect of running — lockfiles the install step writes,
    /// build artefacts under `dist/` / `build/` / `.egg-info/`,
    /// `__pycache__/`, `.venv/`, etc. These paths are excluded from
    /// both task cache keys and dish signatures, so they can't
    /// contaminate the cache-hit invariant (same source → same key).
    ///
    /// Rule of thumb: if the file wouldn't exist on a pristine clone
    /// and its content is reproducible from the declared inputs,
    /// declare it here. Anything a user would conceptually commit
    /// (including intentionally-committed lockfiles for reproducible
    /// deps) should stay OUT of this list — fingerprint covers those.
    ///
    /// Default: empty — adapters without side-effect writes inherit
    /// this and the filter is a no-op.
    fn derived_paths(&self) -> Vec<String> {
        Vec::new()
    }

    /// Parse the project's required toolchain version, if declared.
    ///
    /// Returns `Ok(None)` when no version is pinned (the adapter's best
    /// effort; bento falls back to the system toolchain).
    fn required_toolchain(&self, dir: &Path) -> Result<Option<ToolVersion>>;

    /// Install this dish's dependencies. Expected to be idempotent.
    fn install(&self, ctx: &TaskContext) -> Result<()>;

    /// Add one or more packages as dependencies of this dish.
    ///
    /// Each adapter shells out to its native package manager —
    /// `cargo add`, `bun add`, `npm install --save`, `pnpm add`,
    /// `yarn add`, `go get`. The verb is for *adding new deps*, not
    /// upgrading existing ones; the package manager's own resolution
    /// rules pick the version when the input is bare (e.g. `serde` →
    /// the latest matching the project's MSRV / engines).
    ///
    /// Default impl returns an error so adapters opt in. Returning an
    /// error from this default is the right behaviour: an unrecognised
    /// dish should fail loud rather than silently no-op.
    fn add(&self, _ctx: &TaskContext, _packages: &[&str], _opts: AddOptions) -> Result<Vec<Added>> {
        anyhow::bail!(
            "adapter `{}` doesn't support `bento add` yet — \
             fall back to the native package manager for now",
            self.id()
        )
    }

    /// Cheap filesystem probe: are this dish's dependencies present
    /// enough that task runs won't immediately fail on `require()` /
    /// `import` / link errors?
    ///
    /// Called once per dish per `bento ci` invocation, before any
    /// task runs. File-existence checks only — no hashing, no
    /// subprocess calls.
    ///
    /// Default: [`InstallProbe::Ready`] — adapters without a local
    /// install footprint (go, cargo, maven, gradle — all rely on
    /// global module caches) inherit this and skip the install step
    /// entirely. Node-family and PHP adapters override.
    fn install_probe(&self, _dir: &Path) -> InstallProbe {
        InstallProbe::Ready
    }

    /// Default tasks supplied by this adapter (merged with `dish.toml`).
    fn default_tasks(&self) -> Vec<DefaultTask>;

    /// Tasks discovered from project metadata in `dir`. Used by
    /// `bento init` / `bento dish add` to pre-populate `dish.toml` with
    /// real script wiring rather than relying purely on adapter defaults.
    ///
    /// Returning `None` (the default) means "no per-project tasks to
    /// surface — defaults are sufficient." Returning `Some(vec![])` is
    /// distinct: it says "I looked, and the project has no scripts."
    fn detected_tasks(&self, _dir: &Path) -> Option<Vec<DetectedTask>> {
        None
    }

    /// Fingerprint the *installed* toolchain version so patch-level
    /// drift underneath a declared pin (`go.mod` says `go 1.22`, but
    /// the host has 1.22.3 one day and 1.22.5 the next) invalidates
    /// the cache. Default: `None` (declared version is the only input).
    ///
    /// Called once per adapter per `bento plan` / `bento ci` process —
    /// the result is memoised so the subprocess cost is paid at most
    /// once per tool. The returned string is opaque; we just hash it.
    fn resolved_toolchain_fingerprint(&self) -> Option<String> {
        None
    }

    /// Declare how this adapter's task can produce structured
    /// diagnostics on failure. Returning `None` (the default) means
    /// bento doesn't try — the task's stderr is the only failure
    /// signal.
    ///
    /// When a task fails AND this returns `Some(hook)`, the executor
    /// re-runs the task with the hook's [`crate::DiagnosticRerun`]
    /// applied, captures the output, and dispatches to the parser
    /// declared in the hook's [`crate::DiagnosticParser`] — either
    /// a built-in via [`crate::ParserId`] or back to the adapter
    /// itself via [`Self::parse_diagnostics`] (used by subprocess
    /// plugin adapters with custom output formats).
    fn diagnostic_hook(&self, _task_name: &str) -> Option<crate::DiagnosticHook> {
        None
    }

    /// Parse captured diagnostic re-run output for a task. Only
    /// invoked when [`Self::diagnostic_hook`] returned a hook with
    /// `parser = DiagnosticParser::Plugin`.
    ///
    /// Built-in adapters never declare `Plugin` parser and inherit
    /// the empty default. The subprocess plugin shim overrides this
    /// to make the `parseDiagnostics` RPC.
    ///
    /// Plugin authors implement this in their plugin binary and
    /// declare `parser: "plugin"` in their manifest's
    /// `diagnostic_hooks` map. See `docs/plugins.md`.
    fn parse_diagnostics(
        &self,
        _task_name: &str,
        _stdout: &str,
        _stderr: &str,
        _dish_dir: &Path,
        _workspace_root: &Path,
    ) -> Vec<crate::Diagnostic> {
        Vec::new()
    }
}
