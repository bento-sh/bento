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
        ToolchainAction::Pin { pin } => print_pin_advice(&pin),
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

    if planned.is_empty() {
        if global.json {
            println!("{}", serde_json::json!({ "installed": [] }));
        } else {
            println!("no toolchain pins found in this workspace");
            println!("(set [toolchain] in bento.toml to opt in)");
        }
        return Ok(0);
    }

    if !global.json {
        println!("installing {} toolchain pin(s)…", planned.len());
        for ((tool, version), dishes) in &planned {
            println!("  {tool}@{version}  (used by: {})", dishes.join(", "));
        }
        println!();
    }

    let mut installed = Vec::new();
    let mut failed = 0u32;
    for ((tool, version), dishes) in &planned {
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
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "installed": installed,
                "failed": failed,
            }))?
        );
    }

    Ok(if failed > 0 { 1 } else { 0 })
}

// ── list ──────────────────────────────────────────────────────────

fn list(global: &GlobalFlags) -> Result<i32> {
    let store_root = match Store::default_root() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("could not determine toolchain root: {e}");
            return Ok(1);
        }
    };
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
        println!("{}", serde_json::to_string_pretty(&json)?);
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

// ── pin (stub for now) ────────────────────────────────────────────

fn print_pin_advice(pin: &str) -> Result<i32> {
    eprintln!(
        "`bento toolchain pin` is not implemented yet (preserves your bento.toml \
formatting safely lands in a future release)."
    );
    eprintln!();
    eprintln!("For now, edit bento.toml directly:");
    eprintln!();
    eprintln!("    [toolchain]");
    if let Some((tool, version)) = pin.split_once('=') {
        eprintln!("    {tool} = \"{version}\"");
    } else {
        eprintln!("    # supply as <tool>=<version>, e.g. go=\"1.22.3\"");
    }
    Ok(2)
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
