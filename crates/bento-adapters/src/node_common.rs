//! Shared primitives for the Node-family adapters (npm, pnpm, yarn, bun).
//!
//! Node version resolution priority (first hit wins):
//!
//! 1. `.nvmrc` — nvm convention, also honoured by fnm and most CI Node
//!    setup actions.
//! 2. `.node-version` — older convention (nodenv), still common.
//! 3. `.tool-versions` line for `nodejs` (asdf/mise convention; also
//!    accepts the alias `node` for plugins that don't follow asdf-nodejs).
//! 4. `volta.node` in `package.json` — Volta toolchain manager.
//! 5. `engines.node` in `package.json` — npm publishing convention; rare
//!    in apps but common in libraries.
//! 6. `@types/node` major version in `package.json` (devDependencies or
//!    dependencies). De-facto pseudo-pin in modern TS projects — the
//!    @types/node major tracks the Node major it covers. Returned as
//!    `^N` so users can pin tighter if they want.
//!
//! Keeping the parsers in one module means a fix to (say) the `v`-prefix
//! handling lands everywhere at once.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::adapter::{DetectedTask, ToolVersion};
use crate::tool_versions;

/// Resolve a Node version for a Node-family dish at `dir`. Callers pass
/// the `tool` name they want on the returned [`ToolVersion`] (`"node"`
/// for npm/pnpm/yarn, `"bun"` for bun — bun reads `.bun-version` directly).
pub fn resolve_node_version(dir: &Path, tool: &'static str) -> Result<Option<ToolVersion>> {
    if let Some(v) = read_version_file(&dir.join(".nvmrc"))? {
        return Ok(Some(tool_version(tool, v)));
    }
    if let Some(v) = read_version_file(&dir.join(".node-version"))? {
        return Ok(Some(tool_version(tool, v)));
    }
    if let Some(v) = tool_versions::read_tool_version(dir, &["nodejs", "node"])? {
        return Ok(Some(tool_version(tool, v)));
    }
    let pkg_json = dir.join("package.json");
    if let Some(v) = read_volta_node(&pkg_json)? {
        return Ok(Some(tool_version(tool, v)));
    }
    if let Some(v) = read_engines_node(&pkg_json, tool)? {
        return Ok(Some(v));
    }
    if let Some(v) = read_types_node_major(&pkg_json)? {
        return Ok(Some(tool_version(tool, v)));
    }
    Ok(None)
}

pub fn read_version_file(path: &Path) -> Result<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let line = raw.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        return Ok(None);
    }
    // `.nvmrc` convention allows `v22.1.0` or `22.1.0`; accept both.
    Ok(Some(line.strip_prefix('v').unwrap_or(line).to_string()))
}

pub fn tool_version(tool: &'static str, version: String) -> ToolVersion {
    ToolVersion {
        tool: tool.to_string(),
        version,
    }
}

fn read_engines_node(pkg_json: &Path, tool: &'static str) -> Result<Option<ToolVersion>> {
    let Some(value) = read_package_json(pkg_json)? else {
        return Ok(None);
    };
    let Some(node) = value
        .get("engines")
        .and_then(|e| e.get("node"))
        .and_then(|n| n.as_str())
    else {
        return Ok(None);
    };
    let trimmed = node.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(tool_version(tool, trimmed.to_string())))
}

/// Infer a Node major version from the project's `@types/node`
/// dependency. The package version of `@types/node` tracks the Node
/// major it covers (e.g. `@types/node@24.x.x` → Node 24). Reads from
/// `devDependencies` first, then `dependencies`.
///
/// Returns `^N` (semver-major-compatible) rather than the exact pinned
/// types version — agents can fix code, but the Node runtime they
/// target is the major. Users wanting tighter pins set `.nvmrc` or
/// `[toolchain] node = "..."` explicitly.
fn read_types_node_major(pkg_json: &Path) -> Result<Option<String>> {
    let Some(value) = read_package_json(pkg_json)? else {
        return Ok(None);
    };
    let constraint = value
        .get("devDependencies")
        .and_then(|d| d.get("@types/node"))
        .or_else(|| value.get("dependencies").and_then(|d| d.get("@types/node")))
        .and_then(|v| v.as_str());
    let Some(constraint) = constraint else {
        return Ok(None);
    };
    Ok(parse_types_node_major(constraint))
}

/// Pull the major version out of an npm dep constraint. Accepts
/// `^24.10.1`, `~24.0.0`, `>=24`, `24.10.1`, `24.x`, `latest` (→ None).
/// Returns `^N` for a parseable major; `None` for floats / wildcards.
fn parse_types_node_major(constraint: &str) -> Option<String> {
    let trimmed = constraint.trim();
    // Skip the leading semver operator if present.
    let after_op = trimmed
        .trim_start_matches(['^', '~', '>', '<', '=', ' '])
        .trim_start();
    // First numeric run is the major.
    let major: String = after_op
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if major.is_empty() {
        None
    } else {
        Some(format!("^{major}"))
    }
}

/// Read a Volta-pinned Node version from `package.json` —
/// `{"volta": {"node": "20.10.0"}}`.
fn read_volta_node(pkg_json: &Path) -> Result<Option<String>> {
    let Some(value) = read_package_json(pkg_json)? else {
        return Ok(None);
    };
    let Some(node) = value
        .get("volta")
        .and_then(|v| v.get("node"))
        .and_then(|n| n.as_str())
    else {
        return Ok(None);
    };
    let trimmed = node.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

/// Walk up from `dir`'s parent looking for the nearest ancestor that
/// declares a JS workspace — `package.json` with a `"workspaces"` field
/// (npm / yarn / bun) or a `pnpm-workspace.yaml` (pnpm). Returns the
/// ancestor's path when found; `None` otherwise.
///
/// Deliberately skips `dir` itself: a workspace-root dish IS its own
/// install scope, which the [`LanguageAdapter::install_scope`] default
/// (`dir`) already returns.
///
/// Used to dedupe `adapter.install()` calls in the executor — every dish
/// inside a shared JS workspace returns the same root, so concurrent
/// dishes pile into one install instead of racing on the hoisted
/// `node_modules` symlinks.
pub fn find_node_workspace_root(dir: &Path) -> Option<PathBuf> {
    let mut current = dir.parent()?;
    loop {
        if current.join("pnpm-workspace.yaml").is_file()
            || package_json_has_workspaces(&current.join("package.json"))
        {
            return Some(current.to_path_buf());
        }
        current = current.parent()?;
    }
}

fn package_json_has_workspaces(pkg_json: &Path) -> bool {
    matches!(
        read_package_json(pkg_json),
        Ok(Some(ref v)) if v.get("workspaces").is_some()
    )
}

fn read_package_json(pkg_json: &Path) -> Result<Option<serde_json::Value>> {
    if !pkg_json.is_file() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(pkg_json)
        .with_context(|| format!("reading {}", pkg_json.display()))?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", pkg_json.display()))?;
    Ok(Some(value))
}

/// Fold a set of (tool, probe) pairs into a single fingerprint string,
/// skipping entries whose probe was `None`. Deterministic: the order
/// of the input slice is preserved in the output. Returns `None` when
/// every probe was empty — the caller should then omit the mix-in.
pub fn combine_probes(pairs: &[(&str, Option<String>)]) -> Option<String> {
    let present: Vec<String> = pairs
        .iter()
        .filter_map(|(name, v)| v.as_ref().map(|ver| format!("{name}={ver}")))
        .collect();
    if present.is_empty() {
        None
    } else {
        Some(present.join(" "))
    }
}

/// Shared eslint diagnostic hook for the Node-family adapters
/// (npm/pnpm/yarn/bun). The user's `lint` task is typically a wrapper
/// like `npm run lint` that calls eslint internally; we replace it
/// outright with `eslint --format=json` so we get parseable output
/// regardless of script wiring.
///
/// Limit: if the user's lint script does more than just eslint
/// (e.g. `prettier --check && eslint .`), the diagnostic re-run only
/// captures eslint's output. That's the lossier-but-still-useful
/// trade — the underlying failure is reported via stderr regardless;
/// diagnostics are strictly additive.
pub fn node_eslint_hook(task: &str) -> Option<crate::diagnostic::DiagnosticHook> {
    use crate::diagnostic::{DiagnosticHook, DiagnosticParser, DiagnosticRerun, ParserId};
    if task != "lint" {
        return None;
    }
    Some(DiagnosticHook {
        rerun: DiagnosticRerun::Replace("eslint --format=json".into()),
        parser: DiagnosticParser::Builtin(ParserId::Eslint),
    })
}

/// Parse the `scripts` block of a `package.json` and emit one
/// [`DetectedTask`] per entry, using `run_prefix` to build the run
/// command (e.g. `"npm run"` → `npm run build`, `"yarn"` → `yarn build`).
///
/// Returns `None` if `package.json` is missing or unparseable; returns
/// `Some(vec![])` if the file exists but declares no scripts. The
/// distinction matters to `bento init` — `None` means "fall back to
/// adapter defaults", `Some(vec![])` means "this project genuinely has
/// no scripts".
///
/// Output order follows package.json's serde-preserved key order (we
/// rely on `serde_json::Value::as_object` returning a `Map<String, Value>`
/// which preserves insertion order with the `preserve_order` feature —
/// otherwise alphabetical, which is fine for a generated config).
pub fn detected_npm_scripts(dir: &Path, run_prefix: &str) -> Option<Vec<DetectedTask>> {
    let pkg_json = dir.join("package.json");
    let value = read_package_json(&pkg_json).ok().flatten()?;
    let scripts = value.get("scripts").and_then(|v| v.as_object())?;
    let tasks: Vec<DetectedTask> = scripts
        .iter()
        .filter_map(|(name, _)| {
            let n = name.trim();
            if n.is_empty() {
                None
            } else {
                Some(DetectedTask {
                    name: n.to_string(),
                    run: format!("{run_prefix} {n}"),
                })
            }
        })
        .collect();
    Some(tasks)
}

/// Input globs every Node-family adapter uses for build/test/lint.
pub fn base_inputs(lockfile: &str) -> Vec<String> {
    vec![
        "src/**".into(),
        "public/**".into(),
        "index.html".into(),
        "package.json".into(),
        lockfile.into(),
        "tsconfig*.json".into(),
        "vite.config.*".into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_workspace_root_returns_none_when_no_marker() {
        let tmp = tempfile::tempdir().unwrap();
        let leaf = tmp.path().join("packages/web");
        std::fs::create_dir_all(&leaf).unwrap();
        // No package.json with workspaces anywhere — leaf is standalone.
        assert_eq!(find_node_workspace_root(&leaf), None);
    }

    #[test]
    fn find_workspace_root_walks_up_to_npm_workspaces_root() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"root","workspaces":["packages/*"]}"#,
        )
        .unwrap();
        let leaf = tmp.path().join("packages/web");
        std::fs::create_dir_all(&leaf).unwrap();
        std::fs::write(leaf.join("package.json"), r#"{"name":"web"}"#).unwrap();
        assert_eq!(
            find_node_workspace_root(&leaf)
                .unwrap()
                .canonicalize()
                .unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn find_workspace_root_recognises_pnpm_workspace_yaml() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("pnpm-workspace.yaml"),
            "packages:\n  - 'packages/*'\n",
        )
        .unwrap();
        let leaf = tmp.path().join("packages/api");
        std::fs::create_dir_all(&leaf).unwrap();
        assert_eq!(
            find_node_workspace_root(&leaf)
                .unwrap()
                .canonicalize()
                .unwrap(),
            tmp.path().canonicalize().unwrap()
        );
    }

    #[test]
    fn find_workspace_root_skips_self_dir() {
        // A workspace-root dish IS its own install scope; the helper
        // walks parents only so the trait default (`dir`) handles it.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"root","workspaces":["packages/*"]}"#,
        )
        .unwrap();
        assert_eq!(find_node_workspace_root(tmp.path()), None);
    }

    #[test]
    fn find_workspace_root_object_form_workspaces_field_works() {
        // `"workspaces": { "packages": [...] }` — yarn berry / npm 7+ form.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"root","workspaces":{"packages":["apps/*"]}}"#,
        )
        .unwrap();
        let leaf = tmp.path().join("apps/foo");
        std::fs::create_dir_all(&leaf).unwrap();
        assert!(find_node_workspace_root(&leaf).is_some());
    }

    #[test]
    fn find_workspace_root_ignores_package_json_without_workspaces() {
        // A bare `package.json` (no workspaces field) is just a sibling
        // package, not a workspace root.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
        let leaf = tmp.path().join("nested");
        std::fs::create_dir_all(&leaf).unwrap();
        assert_eq!(find_node_workspace_root(&leaf), None);
    }

    #[test]
    fn node_eslint_hook_returns_replace_for_lint() {
        use crate::diagnostic::{DiagnosticParser, DiagnosticRerun, ParserId};
        let h = node_eslint_hook("lint").expect("lint should have a hook");
        assert_eq!(h.parser, DiagnosticParser::Builtin(ParserId::Eslint));
        match h.rerun {
            DiagnosticRerun::Replace(s) => assert_eq!(s, "eslint --format=json"),
            _ => panic!("expected Replace"),
        }
    }

    #[test]
    fn node_eslint_hook_none_for_other_tasks() {
        for t in ["build", "test", "install", "migrate"] {
            assert!(node_eslint_hook(t).is_none(), "{t} should not have a hook");
        }
    }

    #[test]
    fn falls_back_to_types_node_major() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"x","devDependencies":{"@types/node":"^24.10.1"}}"#,
        )
        .unwrap();
        let v = resolve_node_version(tmp.path(), "node").unwrap().unwrap();
        assert_eq!(v.version, "^24");
    }

    #[test]
    fn types_node_in_dependencies_works_too() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"x","dependencies":{"@types/node":"~22.0.0"}}"#,
        )
        .unwrap();
        let v = resolve_node_version(tmp.path(), "node").unwrap().unwrap();
        assert_eq!(v.version, "^22");
    }

    #[test]
    fn explicit_signals_beat_types_node_inference() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".nvmrc"), "20.10.0\n").unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"devDependencies":{"@types/node":"^24"}}"#,
        )
        .unwrap();
        let v = resolve_node_version(tmp.path(), "node").unwrap().unwrap();
        assert_eq!(v.version, "20.10.0");
    }

    #[test]
    fn parse_types_node_major_handles_common_constraints() {
        assert_eq!(parse_types_node_major("^24.10.1"), Some("^24".into()));
        assert_eq!(parse_types_node_major("~22.0.0"), Some("^22".into()));
        assert_eq!(parse_types_node_major("20"), Some("^20".into()));
        assert_eq!(parse_types_node_major(">=18"), Some("^18".into()));
        assert_eq!(parse_types_node_major("24.x"), Some("^24".into()));
        assert_eq!(parse_types_node_major("latest"), None);
        assert_eq!(parse_types_node_major("*"), None);
    }

    #[test]
    fn nvmrc_strips_v_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".nvmrc"), "v22.1.0\n").unwrap();
        let v = resolve_node_version(tmp.path(), "node").unwrap().unwrap();
        assert_eq!(v.version, "22.1.0");
    }

    #[test]
    fn falls_back_to_engines_node() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"engines":{"node":"^22.0.0"}}"#,
        )
        .unwrap();
        let v = resolve_node_version(tmp.path(), "node").unwrap().unwrap();
        assert_eq!(v.version, "^22.0.0");
    }

    #[test]
    fn returns_none_with_no_pin() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
        assert!(resolve_node_version(tmp.path(), "node").unwrap().is_none());
    }

    #[test]
    fn falls_back_to_tool_versions_nodejs() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
        std::fs::write(
            tmp.path().join(".tool-versions"),
            "nodejs 22.1.0\nruby 3.2\n",
        )
        .unwrap();
        let v = resolve_node_version(tmp.path(), "node").unwrap().unwrap();
        assert_eq!(v.version, "22.1.0");
    }

    #[test]
    fn falls_back_to_tool_versions_node_alias() {
        let tmp = tempfile::tempdir().unwrap();
        // Some asdf plugins use 'node' instead of 'nodejs'.
        std::fs::write(tmp.path().join(".tool-versions"), "node 22.1.0\n").unwrap();
        let v = resolve_node_version(tmp.path(), "node").unwrap().unwrap();
        assert_eq!(v.version, "22.1.0");
    }

    #[test]
    fn falls_back_to_volta_node() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"x","volta":{"node":"20.10.0"}}"#,
        )
        .unwrap();
        let v = resolve_node_version(tmp.path(), "node").unwrap().unwrap();
        assert_eq!(v.version, "20.10.0");
    }

    #[test]
    fn nvmrc_beats_volta_and_engines() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".nvmrc"), "22.1.0\n").unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"volta":{"node":"20.10.0"},"engines":{"node":"^18.0.0"}}"#,
        )
        .unwrap();
        let v = resolve_node_version(tmp.path(), "node").unwrap().unwrap();
        assert_eq!(v.version, "22.1.0");
    }

    #[test]
    fn tool_versions_beats_volta() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".tool-versions"), "nodejs 22.1.0\n").unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"volta":{"node":"20.10.0"}}"#,
        )
        .unwrap();
        let v = resolve_node_version(tmp.path(), "node").unwrap().unwrap();
        assert_eq!(v.version, "22.1.0");
    }

    #[test]
    fn volta_beats_engines() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"volta":{"node":"20.10.0"},"engines":{"node":"^18.0.0"}}"#,
        )
        .unwrap();
        let v = resolve_node_version(tmp.path(), "node").unwrap().unwrap();
        assert_eq!(v.version, "20.10.0");
    }

    #[test]
    fn tool_name_is_respected() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".nvmrc"), "22.1.0\n").unwrap();
        let v = resolve_node_version(tmp.path(), "bun").unwrap().unwrap();
        assert_eq!(v.tool, "bun");
    }

    #[test]
    fn detected_npm_scripts_returns_one_task_per_script() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"scripts":{"build":"vite build","test":"vitest","lint":"eslint ."}}"#,
        )
        .unwrap();
        let tasks = detected_npm_scripts(tmp.path(), "npm run").unwrap();
        let names: Vec<_> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"build"));
        assert!(names.contains(&"test"));
        assert!(names.contains(&"lint"));
        let build = tasks.iter().find(|t| t.name == "build").unwrap();
        assert_eq!(build.run, "npm run build");
    }

    #[test]
    fn detected_npm_scripts_handles_colon_names() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"scripts":{"test:coverage":"vitest --coverage"}}"#,
        )
        .unwrap();
        let tasks = detected_npm_scripts(tmp.path(), "npm run").unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].name, "test:coverage");
        assert_eq!(tasks[0].run, "npm run test:coverage");
    }

    #[test]
    fn detected_npm_scripts_uses_run_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"scripts":{"build":"webpack"}}"#,
        )
        .unwrap();
        let yarn_tasks = detected_npm_scripts(tmp.path(), "yarn").unwrap();
        assert_eq!(yarn_tasks[0].run, "yarn build");

        let bun_tasks = detected_npm_scripts(tmp.path(), "bun run").unwrap();
        assert_eq!(bun_tasks[0].run, "bun run build");
    }

    #[test]
    fn detected_npm_scripts_returns_empty_when_no_scripts_block() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
        // No scripts block at all -> None (caller falls back to defaults).
        assert!(detected_npm_scripts(tmp.path(), "npm run").is_none());
    }

    #[test]
    fn detected_npm_scripts_returns_some_empty_for_empty_scripts() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("package.json"),
            r#"{"name":"x","scripts":{}}"#,
        )
        .unwrap();
        let tasks = detected_npm_scripts(tmp.path(), "npm run").unwrap();
        assert!(tasks.is_empty());
    }

    #[test]
    fn detected_npm_scripts_returns_none_when_package_json_missing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(detected_npm_scripts(tmp.path(), "npm run").is_none());
    }
}
