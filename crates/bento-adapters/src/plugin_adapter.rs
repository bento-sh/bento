//! [`SubprocessAdapter`] — wraps a `bento_plugin::Client` to expose an
//! out-of-process plugin as a [`LanguageAdapter`].
//!
//! Trait methods take `&self` but every RPC needs exclusive access to
//! the stdio channel — hence the `Mutex<Client>`. Calls are serialised
//! per design-doc Decisions §3 so a `Mutex` (vs `RwLock`) is right;
//! contention is bounded by the number of concurrent dishes querying
//! the same plugin and is dust compared to the RPC roundtrip itself.

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use std::collections::BTreeMap;

use bento_plugin::wire::{
    Capabilities, DefaultTask as WireTask, DiagnosticPayload, LogLevel, LogParams, LogStream,
    ManifestDiagnosticHook, ManifestRerun, ParseDiagnosticsParams, ParseDiagnosticsResult,
    ToolVersion as WireToolVersion,
};
use bento_plugin::{
    Client, NoopNotifier, Notifier, DEFAULT_INSTALL_TIMEOUT, DEFAULT_QUERY_TIMEOUT,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::adapter::{DefaultTask, LanguageAdapter, TaskContext, ToolVersion};
use crate::diagnostic::{DiagnosticHook, DiagnosticParser, DiagnosticRerun, ParserId};
use crate::diagnostic_record::{Diagnostic, Severity};

pub struct SubprocessAdapter {
    inner: Mutex<Client>,
    // Mirrored manifest fields. `id()` and `display_name()` need to hand
    // out `&str` borrows; we can't borrow through the Mutex without a
    // guard, so cache the strings here.
    adapter_id: String,
    display_name: String,
    fingerprint: Vec<String>,
    default_tasks: Vec<DefaultTask>,
    capabilities: Capabilities,
    /// Pre-translated diagnostic hooks indexed by task name. None for
    /// hook entries the manifest declared with an unknown parser id —
    /// we drop them with a warning at construction.
    diagnostic_hooks: BTreeMap<String, DiagnosticHook>,
}

impl SubprocessAdapter {
    /// Take ownership of an already-handshook client.
    pub fn from_client(client: Client) -> Self {
        let manifest = client.manifest();
        let default_tasks = manifest
            .default_tasks
            .iter()
            .map(wire_task_to_trait)
            .collect();
        let adapter_id = manifest.adapter_id.clone();
        let diagnostic_hooks = manifest
            .diagnostic_hooks
            .iter()
            .filter_map(|(task, hook)| {
                translate_hook(&adapter_id, task, hook).map(|h| (task.clone(), h))
            })
            .collect();
        Self {
            adapter_id,
            display_name: manifest.display_name.clone(),
            fingerprint: manifest.fingerprint_files.clone(),
            default_tasks,
            capabilities: manifest.capabilities.clone(),
            diagnostic_hooks,
            inner: Mutex::new(client),
        }
    }

    /// Tear down the underlying client (sends `shutdown`, waits, then
    /// kills if needed). Consumes self.
    pub fn shutdown(self) -> Result<()> {
        // Steal the Client out of the Mutex. Mutex::into_inner can fail
        // only if poisoned; in that case we fall through and let Drop
        // hard-kill.
        match self.inner.into_inner() {
            Ok(client) => client.shutdown(),
            Err(_) => Ok(()),
        }
    }
}

impl LanguageAdapter for SubprocessAdapter {
    fn id(&self) -> &str {
        &self.adapter_id
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    fn detect(&self, dir: &Path) -> bool {
        if !self.capabilities.detect {
            return false;
        }
        #[derive(Serialize)]
        struct Params<'a> {
            dir: &'a Path,
        }
        #[derive(Deserialize)]
        struct Resp {
            matches: bool,
        }

        match self.call::<_, Resp>("detect", Some(&Params { dir }), DEFAULT_QUERY_TIMEOUT) {
            Ok(r) => r.matches,
            Err(e) => {
                // detect is called many times during workspace discovery
                // and a misbehaving plugin shouldn't crash the run. Log
                // and treat as a non-match.
                warn!(
                    adapter = %self.adapter_id,
                    error = %e,
                    "plugin detect call failed; treating as non-match"
                );
                false
            }
        }
    }

    fn fingerprint_files(&self) -> Vec<String> {
        self.fingerprint.clone()
    }

    fn required_toolchain(&self, dir: &Path) -> Result<Option<ToolVersion>> {
        if !self.capabilities.required_toolchain {
            return Ok(None);
        }
        #[derive(Serialize)]
        struct Params<'a> {
            dir: &'a Path,
        }
        let v: Option<WireToolVersion> = self.call(
            "requiredToolchain",
            Some(&Params { dir }),
            DEFAULT_QUERY_TIMEOUT,
        )?;
        Ok(v.map(wire_toolversion_to_trait))
    }

    fn install(&self, ctx: &TaskContext) -> Result<()> {
        if !self.capabilities.install {
            debug!(
                adapter = %self.adapter_id,
                "install capability disabled; skipping"
            );
            return Ok(());
        }
        #[derive(Serialize)]
        struct Params<'a> {
            dish_dir: &'a Path,
            dish_name: &'a str,
        }
        let mut printer = PrintNotifier {
            adapter_id: self.adapter_id.clone(),
        };
        let _: serde_json::Value = self.call_with_notifier(
            "install",
            Some(&Params {
                dish_dir: &ctx.dish_dir,
                dish_name: &ctx.dish_name,
            }),
            DEFAULT_INSTALL_TIMEOUT,
            &mut printer,
        )?;
        Ok(())
    }

    fn default_tasks(&self) -> Vec<DefaultTask> {
        self.default_tasks.clone()
    }

    fn resolved_toolchain_fingerprint(&self) -> Option<String> {
        if !self.capabilities.resolved_toolchain_fingerprint {
            return None;
        }
        match self.call::<(), Option<String>>(
            "resolvedToolchainFingerprint",
            None,
            DEFAULT_QUERY_TIMEOUT,
        ) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    adapter = %self.adapter_id,
                    error = %e,
                    "plugin resolvedToolchainFingerprint call failed; falling back to declared version"
                );
                None
            }
        }
    }

    fn diagnostic_hook(&self, task_name: &str) -> Option<DiagnosticHook> {
        self.diagnostic_hooks.get(task_name).cloned()
    }

    fn parse_diagnostics(
        &self,
        task_name: &str,
        stdout: &str,
        stderr: &str,
        dish_dir: &Path,
        workspace_root: &Path,
    ) -> Vec<Diagnostic> {
        let params = ParseDiagnosticsParams {
            task_name: task_name.to_string(),
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            exit_status: 1, // We only get here on failure; specific code is in the report.
            dish_dir: dish_dir.to_string_lossy().into_owned(),
            workspace_root: workspace_root.to_string_lossy().into_owned(),
        };
        match self.call::<_, ParseDiagnosticsResult>(
            "parseDiagnostics",
            Some(&params),
            DEFAULT_QUERY_TIMEOUT,
        ) {
            Ok(resp) => resp
                .diagnostics
                .into_iter()
                .map(payload_to_diagnostic)
                .collect(),
            Err(e) => {
                warn!(
                    adapter = %self.adapter_id,
                    task = %task_name,
                    error = %e,
                    "plugin parseDiagnostics call failed; no diagnostics captured"
                );
                Vec::new()
            }
        }
    }
}

impl SubprocessAdapter {
    fn call<P, R>(&self, method: &str, params: Option<&P>, timeout: Duration) -> Result<R>
    where
        P: Serialize,
        R: for<'de> Deserialize<'de>,
    {
        let mut guard = self.inner.lock().expect("plugin client mutex poisoned");
        guard.call::<P, R, NoopNotifier>(method, params, timeout, &mut NoopNotifier)
    }

    fn call_with_notifier<P, R, N>(
        &self,
        method: &str,
        params: Option<&P>,
        timeout: Duration,
        notifier: &mut N,
    ) -> Result<R>
    where
        P: Serialize,
        R: for<'de> Deserialize<'de>,
        N: Notifier + ?Sized,
    {
        let mut guard = self.inner.lock().expect("plugin client mutex poisoned");
        guard.call(method, params, timeout, notifier)
    }
}

fn wire_task_to_trait(t: &WireTask) -> DefaultTask {
    DefaultTask {
        name: t.name.clone(),
        run: t.run.clone(),
        inputs: t.inputs.clone(),
        outputs: t.outputs.clone(),
    }
}

/// Translate a manifest-declared diagnostic hook into the in-process
/// representation. Returns `None` (with a warn log) when the parser
/// id is unknown — we can't dispatch what we can't name.
fn translate_hook(
    adapter_id: &str,
    task: &str,
    hook: &ManifestDiagnosticHook,
) -> Option<DiagnosticHook> {
    let parser = match hook.parser.as_str() {
        "plugin" => DiagnosticParser::Plugin,
        "cargo-message" => DiagnosticParser::Builtin(ParserId::CargoMessage),
        "golangci-lint" => DiagnosticParser::Builtin(ParserId::GolangciLint),
        "eslint" => DiagnosticParser::Builtin(ParserId::Eslint),
        "ruff" => DiagnosticParser::Builtin(ParserId::Ruff),
        other => {
            warn!(
                adapter = %adapter_id,
                task = %task,
                parser = %other,
                "plugin declared a diagnostic hook with unknown parser id; dropping"
            );
            return None;
        }
    };
    let rerun = match &hook.rerun {
        ManifestRerun::AppendArgs { args } => DiagnosticRerun::AppendArgs(args.clone()),
        ManifestRerun::Replace { command } => DiagnosticRerun::Replace(command.clone()),
    };
    Some(DiagnosticHook { rerun, parser })
}

fn payload_to_diagnostic(p: DiagnosticPayload) -> Diagnostic {
    let severity = match p.severity.as_str() {
        "error" => Severity::Error,
        "warning" => Severity::Warning,
        "info" => Severity::Info,
        "hint" => Severity::Hint,
        other => {
            warn!(severity = %other, "plugin emitted unknown severity, defaulting to warning");
            Severity::Warning
        }
    };
    let mut d = Diagnostic::new(p.file, p.line, severity, p.message, p.source);
    if let Some(c) = p.col {
        d = d.with_col(c);
    }
    if let (Some(le), Some(ce)) = (p.end_line, p.end_col) {
        d = d.with_range(le, ce);
    }
    if let Some(r) = p.rule {
        d = d.with_rule(r);
    }
    d
}

fn wire_toolversion_to_trait(v: WireToolVersion) -> ToolVersion {
    ToolVersion {
        tool: v.tool,
        version: v.version,
    }
}

/// Forwards `notifications/log` from a plugin's `install` call to the
/// user's terminal. Tool-subprocess output (`stream = stdout|stderr`) is
/// passed through verbatim; plugin-internal logs are routed through
/// `tracing` with the adapter as a target tag so they're filterable.
struct PrintNotifier {
    adapter_id: String,
}

impl Notifier for PrintNotifier {
    fn on_log(&mut self, params: &LogParams) {
        match params.stream {
            Some(LogStream::Stdout) => print!("{}", params.message),
            Some(LogStream::Stderr) => eprint!("{}", params.message),
            None => match params.level {
                LogLevel::Error => {
                    tracing::error!(target: "bento.plugin", adapter = %self.adapter_id, "{}", params.message)
                }
                LogLevel::Warn => {
                    tracing::warn!(target: "bento.plugin", adapter = %self.adapter_id, "{}", params.message)
                }
                LogLevel::Info => {
                    tracing::info!(target: "bento.plugin", adapter = %self.adapter_id, "{}", params.message)
                }
                LogLevel::Debug => {
                    tracing::debug!(target: "bento.plugin", adapter = %self.adapter_id, "{}", params.message)
                }
                LogLevel::Trace => {
                    tracing::trace!(target: "bento.plugin", adapter = %self.adapter_id, "{}", params.message)
                }
            },
        }
    }
}
