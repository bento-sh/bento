//! Dependency graph for the dishes inside a bento.
//!
//! A dish's `depends_on` field in `dish.toml` names other dishes (by their
//! `name`, not path). Those references define a DAG, scoped to each bento:
//! a dish only "sees" dependencies that are also listed in the same
//! `bentos/<name>.toml`. Cross-bento dependencies are intentionally
//! rejected — they would make deploy units non-self-contained.
//!
//! The graph is built once at the top of a `plan` or `ci` invocation and
//! used to (a) run independent dishes in parallel and (b) refuse to start
//! a dependent dish until all its deps have finished successfully.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use bento_config::Workspace;

/// Result of topologically sorting a bento's dish DAG.
///
/// `levels` is a Kahn-style layering: each inner `Vec<String>` holds dish
/// names whose dependencies are *all* in earlier levels. Within a level,
/// dishes have no ordering constraint and can be executed in parallel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BentoGraph {
    pub bento: String,
    pub levels: Vec<Vec<String>>,
}

impl BentoGraph {
    /// Every dish name referenced by this graph, in the topo-sorted order
    /// they would run (level-major). Useful for tests and graph printers.
    pub fn flattened(&self) -> Vec<&str> {
        self.levels
            .iter()
            .flat_map(|l| l.iter().map(String::as_str))
            .collect()
    }

    /// Total number of dishes in the graph. Equal to the sum of level sizes.
    pub fn dish_count(&self) -> usize {
        self.levels.iter().map(|l| l.len()).sum()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    /// A dish names a dependency that isn't part of the same bento.
    #[error(
        "dish '{dish}' in bento '{bento}' depends_on '{missing}', which is not a dish of this bento"
    )]
    UnknownDep {
        bento: String,
        dish: String,
        missing: String,
    },

    /// Cycles are reported with the members of every cycle component so
    /// the user can localise the fix.
    #[error(
        "cycle detected in bento '{bento}' among dishes: {}",
        cycle.join(" → ")
    )]
    Cycle { bento: String, cycle: Vec<String> },
}

/// Build the DAG for a single bento and return its topologically-layered
/// execution plan.
pub fn build(workspace: &Workspace, bento_name: &str) -> Result<BentoGraph, GraphError> {
    let bento = workspace
        .bentos
        .get(bento_name)
        .expect("caller must hand in a bento that belongs to this workspace");

    // Dish names included in *this* bento. Only refs within this set are valid.
    let mut dishes_in_bento: BTreeSet<String> = BTreeSet::new();
    let mut name_by_ref: BTreeMap<String, String> = BTreeMap::new();
    for dish_ref in &bento.config.dishes {
        let loaded = workspace
            .dishes_by_path
            .get(std::path::Path::new(dish_ref))
            .expect("workspace load guaranteed this reference resolves");
        dishes_in_bento.insert(loaded.config.name.clone());
        name_by_ref.insert(dish_ref.clone(), loaded.config.name.clone());
    }

    // Build reverse-adjacency (dep → dependents) + in-degrees.
    //
    // `deps[name]` is the set of dishes `name` waits on;
    // `dependents[name]` is the set of dishes that wait on `name`.
    let mut deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut dependents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for dish_name in &dishes_in_bento {
        deps.entry(dish_name.clone()).or_default();
        dependents.entry(dish_name.clone()).or_default();
    }

    for dish_ref in &bento.config.dishes {
        let loaded = &workspace.dishes_by_path[std::path::Path::new(dish_ref)];
        let dish_name = &loaded.config.name;
        for dep_name in &loaded.config.depends_on {
            if !dishes_in_bento.contains(dep_name) {
                return Err(GraphError::UnknownDep {
                    bento: bento_name.to_string(),
                    dish: dish_name.clone(),
                    missing: dep_name.clone(),
                });
            }
            deps.get_mut(dish_name).unwrap().insert(dep_name.clone());
            dependents
                .get_mut(dep_name)
                .unwrap()
                .insert(dish_name.clone());
        }
    }

    // Kahn: seed the queue with zero-in-degree dishes, peel layer by layer.
    let mut levels: Vec<Vec<String>> = Vec::new();
    let mut remaining: BTreeMap<String, usize> =
        deps.iter().map(|(k, v)| (k.clone(), v.len())).collect();

    let mut ready: VecDeque<String> = remaining
        .iter()
        .filter(|&(_, n)| *n == 0)
        .map(|(k, _)| k.clone())
        .collect();

    while !ready.is_empty() {
        // Snapshot the current ready set as one level.
        let mut level: Vec<String> = ready.drain(..).collect();
        level.sort();
        for name in &level {
            remaining.remove(name);
            for dependent in &dependents[name] {
                if let Some(n) = remaining.get_mut(dependent) {
                    *n -= 1;
                    if *n == 0 {
                        ready.push_back(dependent.clone());
                    }
                }
            }
        }
        levels.push(level);
    }

    if !remaining.is_empty() {
        // Whatever's left is in one or more cycles. Surface them all.
        let cycle: Vec<String> = remaining.keys().cloned().collect();
        return Err(GraphError::Cycle {
            bento: bento_name.to_string(),
            cycle,
        });
    }

    Ok(BentoGraph {
        bento: bento_name.to_string(),
        levels,
    })
}

// ── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Builds a workspace with the shape `apps/<name>/dish.toml` for each
    /// `(name, depends_on)` entry. Every dish goes into a single bento
    /// called "prod".
    fn workspace_with_deps(dishes: &[(&str, &[&str])]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("bentos")).unwrap();

        let dish_refs: Vec<String> = dishes
            .iter()
            .map(|(name, _)| format!("apps/{name}"))
            .collect();
        let refs_toml = dish_refs
            .iter()
            .map(|d| format!(r#""{d}""#))
            .collect::<Vec<_>>()
            .join(", ");
        std::fs::write(
            root.join("bentos/prod.toml"),
            format!("name = \"prod\"\ndishes = [{refs_toml}]\n"),
        )
        .unwrap();

        for (name, deps) in dishes {
            let dir = root.join(format!("apps/{name}"));
            std::fs::create_dir_all(&dir).unwrap();
            let deps_toml: String = if deps.is_empty() {
                String::new()
            } else {
                let list = deps
                    .iter()
                    .map(|d| format!(r#""{d}""#))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("depends_on = [{list}]\n")
            };
            std::fs::write(
                dir.join("dish.toml"),
                format!("name = \"{name}\"\n{deps_toml}"),
            )
            .unwrap();
        }

        tmp
    }

    #[test]
    fn linear_chain_produces_single_dish_per_level() {
        // api ← web ← worker
        let tmp = workspace_with_deps(&[("api", &[]), ("web", &["api"]), ("worker", &["web"])]);
        let ws = Workspace::load(tmp.path()).unwrap();
        let graph = build(&ws, "prod").unwrap();

        assert_eq!(graph.levels.len(), 3);
        assert_eq!(graph.levels[0], vec!["api"]);
        assert_eq!(graph.levels[1], vec!["web"]);
        assert_eq!(graph.levels[2], vec!["worker"]);
    }

    #[test]
    fn independent_dishes_collapse_into_one_level() {
        let tmp = workspace_with_deps(&[("api", &[]), ("web", &[]), ("worker", &[])]);
        let ws = Workspace::load(tmp.path()).unwrap();
        let graph = build(&ws, "prod").unwrap();

        // All independent → one level containing all three (sorted).
        assert_eq!(graph.levels.len(), 1);
        assert_eq!(graph.levels[0], vec!["api", "web", "worker"]);
    }

    #[test]
    fn diamond_groups_correctly() {
        //     api
        //    /   \
        //  web   cron
        //    \   /
        //     ui
        let tmp = workspace_with_deps(&[
            ("api", &[]),
            ("web", &["api"]),
            ("cron", &["api"]),
            ("ui", &["web", "cron"]),
        ]);
        let ws = Workspace::load(tmp.path()).unwrap();
        let graph = build(&ws, "prod").unwrap();

        assert_eq!(graph.levels.len(), 3);
        assert_eq!(graph.levels[0], vec!["api"]);
        assert_eq!(graph.levels[1], vec!["cron", "web"]);
        assert_eq!(graph.levels[2], vec!["ui"]);
    }

    #[test]
    fn two_cycle_is_rejected() {
        let tmp = workspace_with_deps(&[("a", &["b"]), ("b", &["a"])]);
        let ws = Workspace::load(tmp.path()).unwrap();
        let err = build(&ws, "prod").unwrap_err();
        match err {
            GraphError::Cycle { bento, cycle } => {
                assert_eq!(bento, "prod");
                assert_eq!(cycle.len(), 2);
                assert!(cycle.contains(&"a".to_string()));
                assert!(cycle.contains(&"b".to_string()));
            }
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    #[test]
    fn self_loop_is_rejected_as_cycle() {
        let tmp = workspace_with_deps(&[("a", &["a"])]);
        let ws = Workspace::load(tmp.path()).unwrap();
        let err = build(&ws, "prod").unwrap_err();
        assert!(matches!(err, GraphError::Cycle { .. }));
    }

    #[test]
    fn unknown_dep_reports_friendly_error() {
        let tmp = workspace_with_deps(&[("a", &["ghost"])]);
        let ws = Workspace::load(tmp.path()).unwrap();
        let err = build(&ws, "prod").unwrap_err();
        match err {
            GraphError::UnknownDep {
                bento,
                dish,
                missing,
            } => {
                assert_eq!(bento, "prod");
                assert_eq!(dish, "a");
                assert_eq!(missing, "ghost");
            }
            other => panic!("expected UnknownDep, got {other:?}"),
        }
    }

    #[test]
    fn dish_count_matches_total_dishes() {
        let tmp = workspace_with_deps(&[("api", &[]), ("web", &["api"]), ("cron", &["api"])]);
        let ws = Workspace::load(tmp.path()).unwrap();
        let graph = build(&ws, "prod").unwrap();
        assert_eq!(graph.dish_count(), 3);
    }

    #[test]
    fn dep_that_exists_in_workspace_but_not_this_bento_is_rejected() {
        // Two bentos, "prod" (api only) and "staging" (api + web). web's
        // depends_on = ["api"] is valid in staging but would be invalid
        // in a bento that only contains web.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("bentos")).unwrap();
        std::fs::write(
            root.join("bentos/prod.toml"),
            r#"name = "prod"
dishes = ["apps/web"]"#,
        )
        .unwrap();
        std::fs::write(
            root.join("bentos/staging.toml"),
            r#"name = "staging"
dishes = ["apps/api", "apps/web"]"#,
        )
        .unwrap();
        for name in ["api", "web"] {
            let dir = root.join(format!("apps/{name}"));
            std::fs::create_dir_all(&dir).unwrap();
        }
        std::fs::write(root.join("apps/api/dish.toml"), r#"name = "api""#).unwrap();
        std::fs::write(
            root.join("apps/web/dish.toml"),
            r#"name = "web"
depends_on = ["api"]
"#,
        )
        .unwrap();

        let ws = Workspace::load(root).unwrap();
        // staging resolves fine — both dishes are present.
        build(&ws, "staging").unwrap();
        // prod must reject — 'web' depends on 'api', but prod doesn't
        // include api.
        let err = build(&ws, "prod").unwrap_err();
        assert!(matches!(err, GraphError::UnknownDep { .. }));

        // (quiet the unused-import lint if the test doesn't touch Path)
        let _ = Path::new("");
    }
}
