//! `bento prime` CLI wrapper.
//!
//! Data layer lives in `bento_core::prime` so `bento-mcp` can reuse
//! it. This module just wires the CLI surface: workspace resolution,
//! JSON vs human output, colourised terminal rendering.

use anyhow::Result;
use bento_config::Workspace;
use bento_core::prime::{self, Output};
use bento_core::{MissReason, TaskStatus};

use crate::cli::GlobalFlags;
use crate::style;

pub fn run(global: &GlobalFlags) -> Result<i32> {
    let root = crate::resolve_workspace_root(global)?;
    let workspace = Workspace::load(&root)?;
    let out = prime::compute(&workspace)?;

    if global.json {
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        print_human(&out);
    }
    Ok(0)
}

pub fn print_human(out: &Output) {
    println!(
        "{}",
        style::bold(&format!("workspace  {}", out.workspace_root))
    );

    println!();
    println!(
        "{} {} bento(s), {} dish(es){}{}",
        style::dim("·"),
        out.bentos.len(),
        out.dishes.len(),
        if out.orphan_dishes.is_empty() {
            String::new()
        } else {
            format!(", {} orphan", out.orphan_dishes.len())
        },
        if out.cache.remote_url.is_some() {
            ", remote cache configured"
        } else {
            ""
        },
    );
    if !out.dishes.is_empty() {
        println!();
        for d in &out.dishes {
            let bentos = if d.bentos.is_empty() {
                style::yellow("(none)")
            } else {
                d.bentos.join(", ")
            };
            let integrations = if d.integrations.is_empty() {
                String::new()
            } else {
                format!(" · {}", d.integrations.join(", "))
            };
            println!(
                "  {} {} ({}) → {}{}",
                style::dim("›"),
                style::cyan(&d.name),
                d.language.as_deref().unwrap_or("-"),
                bentos,
                integrations,
            );
        }
    }

    println!();
    println!("{}", style::bold("cache"));
    println!(
        "  local:   {} {}",
        out.cache.local_dir,
        if out.cache.local_exists {
            style::green("(exists)")
        } else {
            style::yellow("(not yet)")
        }
    );
    if let Some(url) = &out.cache.remote_url {
        let token_state = match (&out.cache.remote_token_env, out.cache.remote_token_resolved) {
            (Some(n), true) => format!("{} {}", style::green("✓"), style::dim(n)),
            (Some(n), false) => format!("{} {} not set", style::yellow("!"), style::dim(n)),
            (None, _) => style::dim("(no token)").to_string(),
        };
        println!("  remote:  {url}  {token_state}");
    }

    println!();
    println!(
        "{} {} total · {} hit · {} miss · {} skipped",
        style::bold("plan"),
        out.plan.total_tasks,
        style::green(&out.plan.hits.to_string()),
        style::yellow(&out.plan.misses.to_string()),
        style::dim(&out.plan.skipped.to_string()),
    );
    if !out.plan.preview.is_empty() {
        for t in &out.plan.preview {
            let (marker, tag) = match t.status {
                TaskStatus::CacheHit => (style::green("✓"), style::green("hit ")),
                TaskStatus::CacheMiss => (style::yellow("·"), style::yellow("miss")),
                TaskStatus::NoAdapter => (style::dim("·"), style::dim("n/a ")),
                TaskStatus::SkippedDiffClean => (style::dim("·"), style::dim("skip")),
            };
            let reason = match t.miss_reason {
                Some(MissReason::Uncacheable) => style::dim(" (uncacheable)"),
                Some(MissReason::ForceRerun) => style::dim(" (force)"),
                Some(MissReason::NeverCached) => String::new(),
                None => String::new(),
            };
            println!(
                "  {marker} [{tag}] {dish}:{task}{reason}",
                dish = t.dish,
                task = t.task,
            );
        }
    }

    println!();
    println!("{}", style::bold("next"));
    for (i, step) in out.recommended_next.iter().enumerate() {
        println!("  {}. {step}", i + 1);
    }
}
