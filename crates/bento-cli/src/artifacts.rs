//! `bento artifacts` — list resolved output paths per dish.
//!
//! Data layer lives in `bento_core::artifacts` so `bento-mcp` can
//! reuse it. This module keeps only the CLI-side printing.

use std::collections::BTreeMap;

use anyhow::Result;
use bento_config::Workspace;

use crate::cli::GlobalFlags;
use crate::style;

pub fn run(global: &GlobalFlags) -> Result<i32> {
    let root = crate::resolve_workspace_root(global)?;
    let workspace = Workspace::load(&root)?;
    let by_dish = bento_core::artifacts::collect(&workspace, global.bento.as_deref())?;

    if global.json {
        let payload: BTreeMap<&String, Vec<String>> = by_dish
            .iter()
            .map(|(name, paths)| {
                (
                    name,
                    paths.iter().map(|p| p.display().to_string()).collect(),
                )
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else if by_dish.is_empty() {
        println!(
            "{} no resolved artefacts — check that your dishes declare [outputs] \
             and that you've built them",
            style::yellow("note:")
        );
    } else {
        for (dish, paths) in &by_dish {
            println!("{}", style::cyan(dish));
            for p in paths {
                println!("  {}", p.display());
            }
        }
    }
    Ok(0)
}
