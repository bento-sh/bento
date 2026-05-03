//! `bento artifacts` data layer.
//!
//! Walks each dish's declared `[outputs]` (dish-level plus task-level,
//! deduped) against the file system and returns the resolved absolute
//! paths. Pure read; nothing is built.
//!
//! Output shape: `{dish_name: [absolute_path, ...], ...}`. Dishes with
//! zero resolved paths are omitted so consumers can rely on every
//! value being non-empty.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bento_config::Workspace;
use walkdir::WalkDir;

#[derive(Debug, thiserror::Error)]
pub enum ArtifactsError {
    #[error("no bento named '{name}' (known: {available})")]
    UnknownBento { name: String, available: String },
}

/// Map of `dish_name → [absolute_paths]`. Sorted, deduped per dish.
/// Dishes with zero resolved paths are omitted.
pub fn collect(
    workspace: &Workspace,
    bento_filter: Option<&str>,
) -> Result<BTreeMap<String, Vec<PathBuf>>> {
    // If a bento filter is given, build the set of dish refs that
    // belong to it once up front. Empty means "all dishes everywhere".
    let bento_dish_refs: Option<BTreeSet<&str>> = bento_filter
        .and_then(|name| workspace.bentos.get(name))
        .map(|b| b.config.dishes.iter().map(String::as_str).collect());

    if let Some(name) = bento_filter {
        if !workspace.bentos.contains_key(name) {
            return Err(ArtifactsError::UnknownBento {
                name: name.to_string(),
                available: workspace
                    .bentos
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
            }
            .into());
        }
    }

    let mut out: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for (rel, loaded) in &workspace.dishes_by_path {
        if let Some(refs) = &bento_dish_refs {
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if !refs.contains(rel_str.as_str()) {
                continue;
            }
        }

        let patterns = collect_patterns(&loaded.config);
        if patterns.is_empty() {
            continue;
        }
        let resolved = resolve_patterns(&loaded.dir, &patterns)?;
        if !resolved.is_empty() {
            out.insert(loaded.config.name.clone(), resolved);
        }
    }
    Ok(out)
}

/// Union of dish-level + every task-level `outputs`, in declaration
/// order with later duplicates dropped.
fn collect_patterns(dish: &bento_config::DishConfig) -> Vec<String> {
    let mut patterns: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for p in &dish.outputs {
        if seen.insert(p.clone()) {
            patterns.push(p.clone());
        }
    }
    for task in dish.tasks.values() {
        if let Some(task_outputs) = &task.outputs {
            for p in task_outputs {
                if seen.insert(p.clone()) {
                    patterns.push(p.clone());
                }
            }
        }
    }
    patterns
}

/// Expand each pattern against `dish_dir`. Literal paths (no glob
/// metachars) are included as-is when they exist on disk; globs are
/// matched against every file under `dish_dir` via walkdir.
///
/// Result is sorted + deduped via `BTreeSet`.
fn resolve_patterns(dish_dir: &Path, patterns: &[String]) -> Result<Vec<PathBuf>> {
    let mut hits: BTreeSet<PathBuf> = BTreeSet::new();

    let glob_patterns: Vec<&String> = patterns.iter().filter(|p| is_glob(p)).collect();
    let literal_patterns: Vec<&String> = patterns.iter().filter(|p| !is_glob(p)).collect();

    for p in &literal_patterns {
        let abs = dish_dir.join(p);
        if abs.exists() {
            hits.insert(abs);
        }
    }

    if !glob_patterns.is_empty() {
        let mut builder = globset::GlobSetBuilder::new();
        for p in &glob_patterns {
            builder.add(
                globset::Glob::new(p).with_context(|| format!("compiling output glob `{p}`"))?,
            );
        }
        let matcher = builder.build()?;

        for entry in WalkDir::new(dish_dir).follow_links(false) {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_file() {
                continue;
            }
            if let Ok(rel) = entry.path().strip_prefix(dish_dir) {
                if matcher.is_match(rel) {
                    hits.insert(entry.path().to_path_buf());
                }
            }
        }
    }

    Ok(hits.into_iter().collect())
}

fn is_glob(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[') || s.contains('{')
}

#[cfg(test)]
mod tests {
    use super::*;
    use bento_config::{DishConfig, Task};

    fn write(dir: &Path, rel: &str, contents: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, contents).unwrap();
    }

    fn dish_with(outputs: Vec<&str>, task_outputs: Vec<(&str, Vec<&str>)>) -> DishConfig {
        let tasks: BTreeMap<String, Task> = task_outputs
            .into_iter()
            .map(|(name, outs)| {
                (
                    name.to_string(),
                    Task {
                        run: Some("true".into()),
                        inputs: None,
                        outputs: Some(outs.into_iter().map(String::from).collect()),
                        workspace_outputs: None,
                        env: vec![],
                        retry: 0,
                    },
                )
            })
            .collect();
        DishConfig {
            name: "test".into(),
            outputs: outputs.into_iter().map(String::from).collect(),
            tasks,
            ..Default::default()
        }
    }

    #[test]
    fn literal_path_resolves_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "bin/api", "binary");
        let resolved = resolve_patterns(tmp.path(), &["bin/api".into()]).unwrap();
        assert_eq!(resolved.len(), 1);
        assert!(resolved[0].ends_with("bin/api"));
    }

    #[test]
    fn glob_pattern_expands_to_files() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "dist/a.js", "");
        write(tmp.path(), "dist/b.js", "");
        write(tmp.path(), "dist/c.css", "");
        let resolved = resolve_patterns(tmp.path(), &["dist/*.js".into()]).unwrap();
        let names: Vec<_> = resolved
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, vec!["a.js", "b.js"]);
    }

    #[test]
    fn collect_patterns_unions_dish_and_task_dedupe_in_order() {
        let dish = dish_with(
            vec!["bin/api"],
            vec![
                ("build", vec!["bin/api", "dist/"]),
                ("test", vec!["coverage.out"]),
            ],
        );
        let pats = collect_patterns(&dish);
        assert_eq!(
            pats,
            vec![
                "bin/api".to_string(),
                "dist/".to_string(),
                "coverage.out".to_string(),
            ]
        );
    }

    #[test]
    fn is_glob_recognises_metachars() {
        assert!(is_glob("dist/*.js"));
        assert!(is_glob("**/*.go"));
        assert!(is_glob("file?.txt"));
        assert!(is_glob("file[ab].txt"));
        assert!(is_glob("file{a,b}.txt"));
        assert!(!is_glob("bin/api"));
        assert!(!is_glob("dist/"));
    }
}
