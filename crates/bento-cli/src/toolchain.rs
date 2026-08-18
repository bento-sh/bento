//! `bento toolchain` subcommands — install, list, pin.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use bento_core::{
    Installer, LanguageAdapter, LoadedDish, ResolutionSource, Resolver, Store, Target, Workspace,
};

use crate::cli::ToolchainAction;
use crate::GlobalFlags;

pub fn run(global: &GlobalFlags, action: ToolchainAction) -> Result<i32> {
    match action {
        ToolchainAction::Install => install_all(global),
        ToolchainAction::List => list(global),
        ToolchainAction::Pin { pin: spec } => pin(global, &spec),
    }
}

// ── install ────────────────────────────────────────────────────────

fn install_all(global: &GlobalFlags) -> Result<i32> {
    let root = crate::resolve_workspace_root(global)?;
    let workspace = Workspace::load(&root)?;
    let registry = bento_core::AdapterRegistry::builtin();
    let installer = Installer::builtin().context("initialising toolchain installer")?;
    let target = Target::current()
        .ok_or_else(|| anyhow::anyhow!("unsupported host target — no toolchain to install"))?;

    // Walk every dish, resolve, install when explicitly pinned.
    let mut planned: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (dish_path, dish) in &workspace.dishes_by_path {
        // Only consider dishes in at least one bento with this filter.
        if let Some(bento_filter) = &global.bento {
            let in_filtered_bento = workspace.bentos.values().any(|b| {
                &b.config.name == bento_filter
                    && b.config
                        .dishes
                        .iter()
                        .any(|d| std::path::Path::new(d) == dish_path.as_path())
            });
            if !in_filtered_bento {
                continue;
            }
        }

        let adapter = match resolve_adapter_for_dish(&registry, dish) {
            Some(a) => a,
            None => continue,
        };
        let resolution = match Resolver::resolve(&dish.dir, &dish.config, &workspace.repo, adapter)?
        {
            Some(r) => r,
            None => continue,
        };
        if !matches!(
            resolution.source,
            ResolutionSource::Dish | ResolutionSource::Repo
        ) {
            continue;
        }
        let version = match resolution.version.as_ref() {
            Some(v) => v.clone(),
            None => continue,
        };
        planned
            .entry((resolution.tool.clone(), version))
            .or_default()
            .push(dish.config.name.clone());
    }

    // Expand each planned primary's co-required tools (e.g. python →
    // uv) into a separate plan that gets installed *before* the
    // primaries so the primary's `delegated_ensure` finds its sibling
    // on PATH. Co-required tools follow the same pin-resolution chain
    // as primaries: explicit `[toolchain] <tool> = "..."` overrides the
    // tool's declared default.
    let co_required = collect_co_required(&installer, &workspace.repo.toolchain.pins, &planned);

    if planned.is_empty() && co_required.is_empty() {
        if global.json {
            // Same three keys as a non-empty run — an agent parsing
            // `.skipped` / `.failed` shouldn't have to special-case
            // the empty plan.
            crate::json::emit(&serde_json::json!({
                "installed": [],
                "skipped": [],
                "failed": 0,
            }))?;
        } else {
            println!("no toolchain pins found in this workspace");
            println!("(set [toolchain] in bento.toml to opt in)");
        }
        return Ok(0);
    }

    if !global.json {
        let total = planned.len() + co_required.len();
        println!("installing {total} toolchain pin(s)…");
        for (tool, version, used_by) in &co_required {
            println!(
                "  {tool}@{version}  (co-required for: {})",
                used_by.join(", ")
            );
        }
        for ((tool, version), dishes) in &planned {
            println!("  {tool}@{version}  (used by: {})", dishes.join(", "));
        }
        println!();
    }

    let mut installed = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = 0u32;

    // Co-required pass first. Each successful install gets its bin
    // dir prepended to PATH so subsequent installs (and our own
    // delegated tool subprocesses, e.g. `uv python install`) find
    // the freshly-laid-down binary. Failures here are hard — a
    // primary that depends on a missing co-tool can't succeed.
    for (tool, version, used_by) in &co_required {
        match installer.ensure(tool, version, target) {
            Ok(bin) => {
                if !global.json {
                    println!("✓ {tool}@{version} → {} (co-required)", bin.display());
                }
                prepend_path(&bin);
                installed.push(serde_json::json!({
                    "tool": tool,
                    "version": version,
                    "bin_dir": bin.to_string_lossy(),
                    "co_required_for": used_by,
                }));
            }
            Err(e) => {
                failed += 1;
                if !global.json {
                    eprintln!("✗ {tool}@{version} (co-required) failed: {e}");
                }
                installed.push(serde_json::json!({
                    "tool": tool,
                    "version": version,
                    "co_required_for": used_by,
                    "error": e.to_string(),
                }));
            }
        }
    }

    // Primary pass. Tools without a built-in installer (today: bun,
    // deno) are reported as `skipped` rather than failed so the
    // GitHub Action's bootstrap fallback (setup-bun / curl) can step
    // in without blocking the rest of the plan.
    for ((tool, version), dishes) in &planned {
        if installer.tool(tool).is_none() {
            if !global.json {
                println!(
                    "↷ {tool}@{version} — no built-in installer; \
                     install separately (e.g. via the matching \
                     `actions/setup-{tool}` step or the upstream \
                     install script)"
                );
            }
            skipped.push(serde_json::json!({
                "tool": tool,
                "version": version,
                "used_by": dishes,
                "reason": "no built-in installer in bento-toolchain",
            }));
            continue;
        }

        match installer.ensure(tool, version, target) {
            Ok(bin) => {
                if !global.json {
                    println!("✓ {tool}@{version} → {}", bin.display());
                }
                installed.push(serde_json::json!({
                    "tool": tool,
                    "version": version,
                    "bin_dir": bin.to_string_lossy(),
                    "used_by": dishes,
                }));
            }
            Err(e) => {
                failed += 1;
                if !global.json {
                    eprintln!("✗ {tool}@{version} failed: {e}");
                }
                installed.push(serde_json::json!({
                    "tool": tool,
                    "version": version,
                    "used_by": dishes,
                    "error": e.to_string(),
                }));
            }
        }
    }

    if global.json {
        crate::json::emit(&serde_json::json!({
            "installed": installed,
            "skipped": skipped,
            "failed": failed,
        }))?;
    }

    Ok(if failed > 0 { 1 } else { 0 })
}

/// Walk the primary plan and produce the list of co-required
/// `(tool, version, used_by)` triples that must install ahead of the
/// primaries. `used_by` is the list of primary tools whose
/// `co_required` declaration produced this entry — surfaces in the
/// JSON output so users see *why* a tool they didn't pin shows up.
fn collect_co_required(
    installer: &Installer,
    repo_pins: &std::collections::BTreeMap<String, String>,
    planned: &BTreeMap<(String, String), Vec<String>>,
) -> Vec<(String, String, Vec<String>)> {
    let mut out: BTreeMap<String, (String, Vec<String>)> = BTreeMap::new();
    for (primary_name, _primary_version) in planned.keys() {
        let Some(tool) = installer.tool(primary_name) else {
            continue;
        };
        for co in tool.co_required() {
            // Workspace `[toolchain] <tool> = "..."` pin wins; otherwise
            // the tool's declared default-for-bento-release. Future:
            // honour dish-level pins too once we have a use case.
            let version = repo_pins
                .get(co.tool)
                .cloned()
                .unwrap_or_else(|| co.default_version.to_string());
            out.entry(co.tool.to_string())
                .and_modify(|(_, used_by)| {
                    if !used_by.contains(primary_name) {
                        used_by.push(primary_name.clone());
                    }
                })
                .or_insert_with(|| (version, vec![primary_name.clone()]));
        }
    }
    out.into_iter()
        .map(|(tool, (version, used_by))| (tool, version, used_by))
        .collect()
}

/// Prepend `bin_dir` to the current process's `PATH`. Subsequent
/// `Command::new(...)` calls (and any subshells they spawn) will see
/// the new directory first. Used to make a freshly-installed
/// co-required tool visible to the primary tool's
/// `delegated_ensure` subprocess in the same `bento toolchain
/// install` invocation.
fn prepend_path(bin_dir: &std::path::Path) {
    let cur = std::env::var_os("PATH").unwrap_or_default();
    let mut paths: Vec<std::path::PathBuf> = std::env::split_paths(&cur).collect();
    paths.insert(0, bin_dir.to_path_buf());
    if let Ok(joined) = std::env::join_paths(paths) {
        // Edition 2021 — `set_var` is safe. Bento's CLI is single-
        // threaded at this point (toolchain install runs sequentially);
        // nothing else is mutating PATH concurrently.
        std::env::set_var("PATH", joined);
    }
}

// ── list ──────────────────────────────────────────────────────────

fn list(global: &GlobalFlags) -> Result<i32> {
    // Propagate rather than printing + exit 1: with `--json` the bare
    // stderr line left the caller with an exit code and no envelope.
    let store_root = Store::default_root().context("resolving the toolchain store root")?;
    let store = Store::new(&store_root);
    let entries = store.list()?;

    if global.json {
        let json: Vec<_> = entries
            .iter()
            .map(|(tool, version)| {
                serde_json::json!({
                    "tool": tool,
                    "version": version,
                    "bin_dir": store.bin_dir(tool, version).to_string_lossy(),
                })
            })
            .collect();
        crate::json::emit(&json)?;
    } else if entries.is_empty() {
        println!("no toolchains installed under {}", store_root.display());
        println!("run `bento toolchain install` to fetch the pins from your workspace");
    } else {
        println!("toolchains installed under {}:", store_root.display());
        for (tool, version) in &entries {
            println!("  {tool}@{version}");
        }
    }

    Ok(0)
}

// ── pin ───────────────────────────────────────────────────────────

/// Write `<tool> = "<version>"` into the workspace `bento.toml`'s
/// `[toolchain]` table. `toml_edit` so comments, ordering and
/// formatting elsewhere in the file survive — same round-trip
/// contract as `scaffold::wire_into_bento`.
fn pin(global: &GlobalFlags, spec: &str) -> Result<i32> {
    let (tool, version) = parse_pin(spec)?;
    let root = crate::resolve_workspace_root(global)?;
    let path = root.join("bento.toml");
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let (updated, previous) =
        apply_pin(&raw, tool, version).with_context(|| format!("editing {}", path.display()))?;

    let changed = previous.as_deref() != Some(version);
    if changed {
        std::fs::write(&path, updated).with_context(|| format!("writing {}", path.display()))?;
    }

    if global.json {
        crate::json::emit(&serde_json::json!({
            "tool": tool,
            "version": version,
            "previous": previous,
            "changed": changed,
            "path": path.to_string_lossy(),
        }))?;
        return Ok(0);
    }
    match (&previous, changed) {
        (_, false) => {
            println!(
                "{tool} is already pinned to {version} in {}",
                path.display()
            );
            return Ok(0);
        }
        (Some(p), _) => println!("pinned {tool} = {version} in {} (was {p})", path.display()),
        (None, _) => println!("pinned {tool} = {version} in {}", path.display()),
    }
    if Installer::builtin().is_ok_and(|i| i.tool(tool).is_none()) {
        println!(
            "note: bento has no built-in installer for '{tool}' — the pin feeds the \
             cache key and doctor checks, but the binary has to be on PATH already"
        );
    } else {
        println!("run `bento toolchain install` to fetch it");
    }
    Ok(0)
}

/// Set `[toolchain] <tool> = "<version>"` in `raw`, returning the new
/// document text and whatever was pinned before. `toml_edit` keeps
/// every comment, key order and bit of formatting elsewhere in the
/// file intact — same round-trip contract as
/// `scaffold::wire_into_bento`.
fn apply_pin(raw: &str, tool: &str, version: &str) -> Result<(String, Option<String>)> {
    use toml_edit::{value, DocumentMut, Item, Table};

    let mut doc: DocumentMut = raw.parse().context("parsing TOML")?;
    let table = doc
        .entry("toolchain")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("expected `[toolchain]` to be a table"))?;
    let previous = table.get(tool).and_then(|i| i.as_str()).map(str::to_string);
    table[tool] = value(version);
    Ok((doc.to_string(), previous))
}

fn parse_pin(spec: &str) -> Result<(&str, &str)> {
    match spec.split_once('=') {
        Some((tool, version)) if !tool.trim().is_empty() && !version.trim().is_empty() => {
            Ok((tool.trim(), version.trim()))
        }
        _ => anyhow::bail!("expected `<tool>=<version>` (e.g. `go=1.22.3`), got {spec:?}"),
    }
}

// ── helpers ───────────────────────────────────────────────────────

fn resolve_adapter_for_dish<'a>(
    registry: &'a bento_core::AdapterRegistry,
    dish: &LoadedDish,
) -> Option<&'a dyn LanguageAdapter> {
    if let Some(id) = &dish.config.language {
        return registry.by_id(id);
    }
    registry.detect(&dish.dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn empty_planned() -> BTreeMap<(String, String), Vec<String>> {
        BTreeMap::new()
    }

    fn with_python(version: &str) -> BTreeMap<(String, String), Vec<String>> {
        let mut m = BTreeMap::new();
        m.insert(
            ("python".to_string(), version.to_string()),
            vec!["api".to_string(), "worker".to_string()],
        );
        m
    }

    #[test]
    fn co_required_uses_default_when_unpinned() {
        // python in plan, no uv pin → uv installs at PythonTool's
        // declared default version.
        let installer = Installer::builtin().expect("builtin installer");
        let planned = with_python("3.12");
        let pins = BTreeMap::new();
        let co = collect_co_required(&installer, &pins, &planned);
        assert_eq!(co.len(), 1, "expected exactly one co-required tool");
        let (tool, version, used_by) = &co[0];
        assert_eq!(tool, "uv");
        // The default version should be a non-empty semver-shaped string.
        assert!(!version.is_empty());
        assert_eq!(used_by, &vec!["python".to_string()]);
    }

    #[test]
    fn co_required_honours_repo_pin_override() {
        let installer = Installer::builtin().expect("builtin installer");
        let planned = with_python("3.12");
        let mut pins = BTreeMap::new();
        pins.insert("uv".to_string(), "0.4.27".to_string());
        let co = collect_co_required(&installer, &pins, &planned);
        assert_eq!(co.len(), 1);
        assert_eq!(co[0].0, "uv");
        assert_eq!(co[0].1, "0.4.27", "repo pin should override default");
    }

    #[test]
    fn co_required_empty_when_no_primary_declares_one() {
        // go has no co-required; ensure no spurious entries.
        let installer = Installer::builtin().expect("builtin installer");
        let mut planned = BTreeMap::new();
        planned.insert(
            ("go".to_string(), "1.22.3".to_string()),
            vec!["backend".to_string()],
        );
        let pins = BTreeMap::new();
        let co = collect_co_required(&installer, &pins, &planned);
        assert!(co.is_empty(), "go has no co-required, expected empty");
    }

    #[test]
    fn co_required_empty_for_empty_plan() {
        let installer = Installer::builtin().expect("builtin installer");
        let pins = BTreeMap::new();
        let co = collect_co_required(&installer, &pins, &empty_planned());
        assert!(co.is_empty());
    }

    #[test]
    fn apply_pin_adds_table_and_preserves_the_rest() {
        let raw = "# repo defaults\nname = \"acme\"\n\n[cache]\n# keep me\nremote = \"s3://x\"\n";
        let (out, previous) = apply_pin(raw, "go", "1.22.3").unwrap();
        assert_eq!(previous, None);
        assert!(out.contains("# repo defaults"), "got: {out}");
        assert!(out.contains("# keep me"), "got: {out}");
        assert!(out.contains("remote = \"s3://x\""), "got: {out}");
        let parsed: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(parsed["toolchain"]["go"].as_str(), Some("1.22.3"));
    }

    #[test]
    fn apply_pin_replaces_an_existing_pin_and_reports_the_old_one() {
        let raw = "[toolchain]\n# pinned by CI\ngo = \"1.21.0\"\nnode = \"20.11.0\"\n";
        let (out, previous) = apply_pin(raw, "go", "1.22.3").unwrap();
        assert_eq!(previous.as_deref(), Some("1.21.0"));
        assert!(out.contains("# pinned by CI"), "got: {out}");
        let parsed: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(parsed["toolchain"]["go"].as_str(), Some("1.22.3"));
        assert_eq!(parsed["toolchain"]["node"].as_str(), Some("20.11.0"));
    }

    #[test]
    fn parse_pin_requires_both_halves() {
        assert_eq!(parse_pin("go=1.22.3").unwrap(), ("go", "1.22.3"));
        assert_eq!(parse_pin(" go = 1.22.3 ").unwrap(), ("go", "1.22.3"));
        for bad in ["go", "go=", "=1.22.3", ""] {
            assert!(parse_pin(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn prepend_path_lands_at_front() {
        // Snapshot then restore PATH so we don't pollute other tests.
        let original = std::env::var_os("PATH");
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().to_path_buf();
        prepend_path(&bin);
        let after = std::env::var_os("PATH").unwrap_or_default();
        let first = std::env::split_paths(&after).next().unwrap();
        assert_eq!(first, bin);
        // Restore.
        match original {
            Some(v) => std::env::set_var("PATH", v),
            None => std::env::remove_var("PATH"),
        }
    }
}
