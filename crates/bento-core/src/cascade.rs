//! Pessimistic-correct cache invalidation across `depends_on`.
//!
//! Rule: when dish D depends on X, any change in X's source content must
//! invalidate D's cache. Implemented by folding each dep's *effective
//! signature* into every task key on the dependent.
//!
//! The effective signature for a dish is recursive:
//!
//! ```text
//! effective(D) = hash(content(D), effective(dep) for dep in D.depends_on)
//! ```
//!
//! …unless `D.force_independent = true`, in which case `effective(D) =
//! content(D)` and X's churn does not propagate through D. That's the
//! documented foot-gun: you're promising dependents that your API is
//! stable across the skipped cascade.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use bento_adapters::{AdapterRegistry, IntegrationRegistry, LanguageAdapter};
use bento_config::{DishConfig, Workspace};

use crate::graph::BentoGraph;

/// blake3 digest of a dish's effective (transitive) input content.
pub type DishSig = [u8; 32];

/// Hex-encoded signature. Deliberately returned as a `String` so callers
/// can stream it into the task-key Hasher via `add_extra` without a
/// bytes-to-hex loop at every mix-in site.
pub fn sig_to_hex(sig: &DishSig) -> String {
    let mut s = String::with_capacity(64);
    for b in sig {
        use std::fmt::Write;
        write!(&mut s, "{:02x}", b).unwrap();
    }
    s
}

/// Compute an effective signature for every dish in `graph`. Caller must
/// pass the graph for the bento they're planning/executing — the
/// signature of any dish respects the dep closure within that bento.
pub fn compute(
    workspace: &Workspace,
    graph: &BentoGraph,
    registry: &AdapterRegistry,
    integrations: &IntegrationRegistry,
    env_aliases: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, DishSig>> {
    let mut sigs: BTreeMap<String, DishSig> = BTreeMap::new();

    for level in &graph.levels {
        for dish_name in level {
            let loaded = workspace.dishes_by_name.get(dish_name).with_context(|| {
                format!("dish '{dish_name}' referenced by graph but missing from workspace")
            })?;
            let adapter = resolve_adapter(registry, loaded.config.language.as_deref(), &loaded.dir);
            let tasks = crate::plan::resolve_tasks(
                &loaded.dir,
                &loaded.config,
                adapter,
                &crate::plan::resolve_integrations(integrations, loaded),
            )
            .with_context(|| format!("resolving tasks for dish '{dish_name}'"))?;
            let toolchain = crate::plan::resolve_toolchain_pin(loaded, &workspace.repo, adapter)?;
            let content = content_hash(&DishInputs {
                dish_dir: &loaded.dir,
                dish: &loaded.config,
                adapter,
                tasks: &tasks,
                toolchain: toolchain.as_ref(),
                env_aliases,
            })
            .with_context(|| {
                format!(
                    "hashing content for dish '{dish_name}' at {}",
                    loaded.dir.display()
                )
            })?;

            let effective = if loaded.config.force_independent {
                content
            } else {
                let mut h = blake3::Hasher::new();
                h.update(b"bento-dish-effective-v1");
                h.update(&content);
                // Sort dep names for deterministic order.
                let mut deps: Vec<&String> = loaded.config.depends_on.iter().collect();
                deps.sort();
                for dep in deps {
                    let dep_sig = sigs.get(dep).with_context(|| {
                        format!(
                            "dish '{dish_name}' lists dep '{dep}' that isn't in this graph — \
                             build_graph should have caught this"
                        )
                    })?;
                    h.update(dep_sig);
                }
                h.finalize().into()
            };

            sigs.insert(dish_name.clone(), effective);
        }
    }

    Ok(sigs)
}

/// Build the list of `(dep_name, effective_sig)` pairs that should be
/// mixed into `D`'s task keys. Respects `D.force_independent`.
pub fn deps_for_key<'a>(
    dish: &'a DishConfig,
    signatures: &'a BTreeMap<String, DishSig>,
) -> Vec<(&'a str, &'a DishSig)> {
    if dish.force_independent {
        return Vec::new();
    }
    let mut out: Vec<(&str, &DishSig)> = dish
        .depends_on
        .iter()
        .filter_map(|name| signatures.get(name).map(|sig| (name.as_str(), sig)))
        .collect();
    out.sort_by_key(|(n, _)| *n);
    out
}

fn resolve_adapter<'a>(
    registry: &'a AdapterRegistry,
    language: Option<&str>,
    dir: &Path,
) -> Option<&'a dyn LanguageAdapter> {
    if let Some(id) = language {
        return registry.by_id(id);
    }
    registry.detect(dir)
}

struct DishInputs<'a> {
    dish_dir: &'a Path,
    dish: &'a DishConfig,
    adapter: Option<&'a dyn LanguageAdapter>,
    tasks: &'a [crate::plan::ResolvedTask],
    toolchain: Option<&'a bento_toolchain::Resolution>,
    env_aliases: &'a BTreeMap<String, String>,
}

fn content_hash(input: &DishInputs<'_>) -> Result<DishSig> {
    let DishInputs {
        dish_dir,
        dish,
        adapter,
        tasks,
        toolchain,
        env_aliases,
    } = *input;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"bento-dish-content-v1");
    hasher.update(dish.name.as_bytes());

    // Everything about the dep that its own task keys hash, minus its
    // input files (walked below). Content alone left the dependent
    // hitting cache after the dep's build command, env values,
    // outputs, or toolchain pin changed — the dep rebuilt, the
    // dependent didn't.
    if let Some(r) = toolchain {
        hasher.update(r.source.label().as_bytes());
        hasher.update(r.tool.as_bytes());
        hasher.update(r.version.as_deref().unwrap_or("system").as_bytes());
    }
    if let Some(a) = adapter {
        hasher.update(a.id().as_bytes());
        if let Some(v) = a.required_toolchain(dish_dir)? {
            hasher.update(v.tool.as_bytes());
            hasher.update(v.version.as_bytes());
        }
    }
    for t in tasks {
        for field in [&t.name, &t.run] {
            hasher.update(field.as_bytes());
        }
        for glob in t.outputs.iter().chain(t.workspace_outputs.iter()) {
            hasher.update(glob.as_bytes());
        }
        let mut env_names: Vec<&String> = t.env.iter().collect();
        env_names.sort();
        for name in env_names {
            let source = env_aliases.get(name).map(String::as_str).unwrap_or(name);
            hasher.update(name.as_bytes());
            hasher.update(std::env::var(source).unwrap_or_default().as_bytes());
        }
    }

    // The signature must cover the dish's *source*, which lives in
    // task-level input globs (adapter defaults like `src/**` plus any
    // `[tasks.<name>] inputs` overrides) — dish-level `inputs` is
    // usually empty and `fingerprint_files()` is manifests only.
    // Hashing just those two meant a dep's source edits never moved a
    // dependent's task keys, so `bento ci` green-lit dependents from
    // cache against code they never saw (reported downstream as
    // gosho-app-8knj). Union, not per-task resolution: a superset of
    // every resolved task's inputs is pessimistic-correct, which is
    // this module's contract.
    fn add_glob(globs: &mut Vec<String>, g: String) {
        if !globs.contains(&g) {
            globs.push(g);
        }
    }
    let mut globs: Vec<String> = dish.inputs.clone();
    if let Some(a) = adapter {
        for f in a.fingerprint_files() {
            add_glob(&mut globs, f);
        }
    }
    for t in tasks {
        for g in &t.inputs {
            add_glob(&mut globs, g.clone());
        }
    }

    if globs.is_empty() {
        return Ok(hasher.finalize().into());
    }

    let mut builder = globset::GlobSetBuilder::new();
    for g in &globs {
        builder
            .add(globset::Glob::new(g).with_context(|| format!("compiling dep-sig glob `{g}`"))?);
    }
    let matcher = builder.build()?;

    // Adapter-declared derived paths — excluded from the dish
    // signature for the same reason they're excluded from task cache
    // keys. A change in a bundle-installed Gemfile.lock or a
    // pip-generated egg-info shouldn't cascade-invalidate dependents.
    let derived_matcher = if let Some(a) = adapter {
        let derived = a.derived_paths();
        if derived.is_empty() {
            None
        } else {
            let mut db = globset::GlobSetBuilder::new();
            for g in &derived {
                db.add(
                    globset::Glob::new(g)
                        .with_context(|| format!("compiling derived-paths glob `{g}`"))?,
                );
            }
            Some(db.build()?)
        }
    } else {
        None
    };

    let matched = crate::walk::walk(&crate::walk::FileWalk {
        root: dish_dir,
        include: &matcher,
        exclude: &derived_matcher.iter().collect::<Vec<_>>(),
        respect_ignores: true,
    })?;

    for (rel, is_symlink) in matched {
        let full = dish_dir.join(&rel);
        let content = crate::walk::hashable_content(&full, is_symlink)?;
        // Length-prefix path + content to keep the rolling hash injective.
        let rel_str = rel.to_string_lossy();
        hasher.update(&(rel_str.len() as u64).to_le_bytes());
        hasher.update(rel_str.as_bytes());
        hasher.update(&(content.len() as u64).to_le_bytes());
        hasher.update(&content);
    }

    Ok(hasher.finalize().into())
}

// ── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::build as build_graph;

    fn compute_sigs(
        ws: &Workspace,
        graph: &BentoGraph,
        reg: &AdapterRegistry,
    ) -> Result<BTreeMap<String, DishSig>> {
        compute(
            ws,
            graph,
            reg,
            &IntegrationRegistry::empty(),
            &BTreeMap::new(),
        )
    }

    fn two_dish_fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("bentos")).unwrap();
        std::fs::write(
            root.join("bentos/prod.toml"),
            r#"name = "prod"
dishes = ["lib", "app"]"#,
        )
        .unwrap();

        std::fs::create_dir_all(root.join("lib")).unwrap();
        std::fs::write(
            root.join("lib/dish.toml"),
            r#"name = "lib"
inputs = ["src.txt"]"#,
        )
        .unwrap();
        std::fs::write(root.join("lib/src.txt"), b"v1").unwrap();

        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::write(
            root.join("app/dish.toml"),
            r#"name = "app"
depends_on = ["lib"]
inputs = ["src.txt"]"#,
        )
        .unwrap();
        std::fs::write(root.join("app/src.txt"), b"app-v1").unwrap();

        tmp
    }

    #[test]
    fn dep_change_propagates_to_dependent_signature() {
        let tmp = two_dish_fixture();
        let ws = Workspace::load(tmp.path()).unwrap();
        let graph = build_graph(&ws, "prod").unwrap();
        let reg = AdapterRegistry::builtin();

        let sigs_before = compute_sigs(&ws, &graph, &reg).unwrap();
        std::fs::write(tmp.path().join("lib/src.txt"), b"v2").unwrap();
        let sigs_after = compute_sigs(&ws, &graph, &reg).unwrap();

        assert_ne!(sigs_before["lib"], sigs_after["lib"]);
        assert_ne!(
            sigs_before["app"], sigs_after["app"],
            "dependent must see a new signature when its dep changes"
        );
    }

    #[test]
    fn dep_command_change_propagates_to_dependent_signature() {
        // Regression: the signature hashed the dep's files only, so
        // changing what the dep *builds* (or its outputs / env) left
        // every dependent on a stale cache hit.
        let tmp = two_dish_fixture();
        let sig_for = |run: &str| {
            std::fs::write(
                tmp.path().join("lib/dish.toml"),
                format!(
                    "name = \"lib\"\ninputs = [\"src.txt\"]\n\n[tasks.build]\nrun = \"{run}\"\n"
                ),
            )
            .unwrap();
            let ws = Workspace::load(tmp.path()).unwrap();
            let graph = build_graph(&ws, "prod").unwrap();
            compute_sigs(&ws, &graph, &AdapterRegistry::builtin()).unwrap()
        };
        let before = sig_for("make debug");
        let after = sig_for("make release");
        assert_ne!(before["lib"], after["lib"]);
        assert_ne!(
            before["app"], after["app"],
            "dependent must see a new signature when its dep's build command changes"
        );
    }

    #[test]
    fn force_independent_blocks_propagation() {
        let tmp = two_dish_fixture();
        // Mark app as force_independent.
        std::fs::write(
            tmp.path().join("app/dish.toml"),
            r#"name = "app"
depends_on = ["lib"]
inputs = ["src.txt"]
force_independent = true"#,
        )
        .unwrap();

        let ws = Workspace::load(tmp.path()).unwrap();
        let graph = build_graph(&ws, "prod").unwrap();
        let reg = AdapterRegistry::builtin();

        let sigs_before = compute_sigs(&ws, &graph, &reg).unwrap();
        std::fs::write(tmp.path().join("lib/src.txt"), b"v2").unwrap();
        let sigs_after = compute_sigs(&ws, &graph, &reg).unwrap();

        assert_ne!(sigs_before["lib"], sigs_after["lib"]);
        assert_eq!(
            sigs_before["app"], sigs_after["app"],
            "force_independent dependent must ignore its dep's churn"
        );
    }

    #[test]
    fn force_independent_still_reflects_own_content_changes() {
        let tmp = two_dish_fixture();
        std::fs::write(
            tmp.path().join("app/dish.toml"),
            r#"name = "app"
depends_on = ["lib"]
inputs = ["src.txt"]
force_independent = true"#,
        )
        .unwrap();

        let ws = Workspace::load(tmp.path()).unwrap();
        let graph = build_graph(&ws, "prod").unwrap();
        let reg = AdapterRegistry::builtin();

        let sigs_before = compute_sigs(&ws, &graph, &reg).unwrap();
        std::fs::write(tmp.path().join("app/src.txt"), b"app-v2").unwrap();
        let sigs_after = compute_sigs(&ws, &graph, &reg).unwrap();

        assert_ne!(
            sigs_before["app"], sigs_after["app"],
            "force_independent still honours own-content changes"
        );
    }

    #[test]
    fn independent_dishes_get_distinct_signatures() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("bentos")).unwrap();
        std::fs::write(
            root.join("bentos/prod.toml"),
            r#"name = "prod"
dishes = ["a", "b"]"#,
        )
        .unwrap();
        for (name, payload) in [("a", "aaa"), ("b", "bbb")] {
            std::fs::create_dir_all(root.join(name)).unwrap();
            std::fs::write(
                root.join(format!("{name}/dish.toml")),
                format!("name = \"{name}\"\ninputs = [\"src.txt\"]"),
            )
            .unwrap();
            std::fs::write(root.join(format!("{name}/src.txt")), payload).unwrap();
        }

        let ws = Workspace::load(root).unwrap();
        let graph = build_graph(&ws, "prod").unwrap();
        let reg = AdapterRegistry::builtin();

        let sigs = compute_sigs(&ws, &graph, &reg).unwrap();
        assert_ne!(sigs["a"], sigs["b"]);
    }

    #[test]
    fn dep_source_matched_only_by_adapter_task_inputs_propagates() {
        // The real-world shape of gosho-app-8knj: the dep is a language
        // dish whose dish-level `inputs` is empty — its source is only
        // covered by the adapter's default task inputs (src/**, **/*.py).
        // Editing that source must move the dependent's signature.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("bentos")).unwrap();
        std::fs::write(
            root.join("bentos/prod.toml"),
            r#"name = "prod"
dishes = ["lib", "app"]"#,
        )
        .unwrap();

        std::fs::create_dir_all(root.join("lib/src")).unwrap();
        std::fs::write(
            root.join("lib/dish.toml"),
            r#"name = "lib"
language = "python""#,
        )
        .unwrap();
        std::fs::write(
            root.join("lib/pyproject.toml"),
            b"[project]\nname = \"lib\"\n",
        )
        .unwrap();
        std::fs::write(root.join("lib/src/lib.py"), b"X = 1\n").unwrap();

        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::write(
            root.join("app/dish.toml"),
            r#"name = "app"
depends_on = ["lib"]
inputs = ["src.txt"]"#,
        )
        .unwrap();
        std::fs::write(root.join("app/src.txt"), b"app-v1").unwrap();

        let ws = Workspace::load(root).unwrap();
        let graph = build_graph(&ws, "prod").unwrap();
        let reg = AdapterRegistry::builtin();

        let sigs_before = compute_sigs(&ws, &graph, &reg).unwrap();
        std::fs::write(root.join("lib/src/lib.py"), b"X = 2\n").unwrap();
        let sigs_after = compute_sigs(&ws, &graph, &reg).unwrap();

        assert_ne!(
            sigs_before["lib"], sigs_after["lib"],
            "adapter task-input source must feed the dep's own signature"
        );
        assert_ne!(
            sigs_before["app"], sigs_after["app"],
            "dependent must see a new signature when dep source (matched only \
             by adapter task inputs) changes"
        );
    }

    #[test]
    fn dep_source_matched_only_by_task_input_override_propagates() {
        // Same failure class via `[tasks.<name>] inputs = [...]` declared
        // in dish.toml rather than an adapter default.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir(root.join("bentos")).unwrap();
        std::fs::write(
            root.join("bentos/prod.toml"),
            r#"name = "prod"
dishes = ["lib", "app"]"#,
        )
        .unwrap();

        std::fs::create_dir_all(root.join("lib/data")).unwrap();
        std::fs::write(
            root.join("lib/dish.toml"),
            r#"name = "lib"

[tasks.build]
run = "true"
inputs = ["data/**"]"#,
        )
        .unwrap();
        std::fs::write(root.join("lib/data/seed.txt"), b"v1").unwrap();

        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::write(
            root.join("app/dish.toml"),
            r#"name = "app"
depends_on = ["lib"]
inputs = ["src.txt"]"#,
        )
        .unwrap();
        std::fs::write(root.join("app/src.txt"), b"app-v1").unwrap();

        let ws = Workspace::load(root).unwrap();
        let graph = build_graph(&ws, "prod").unwrap();
        let reg = AdapterRegistry::builtin();

        let sigs_before = compute_sigs(&ws, &graph, &reg).unwrap();
        std::fs::write(root.join("lib/data/seed.txt"), b"v2").unwrap();
        let sigs_after = compute_sigs(&ws, &graph, &reg).unwrap();

        assert_ne!(sigs_before["lib"], sigs_after["lib"]);
        assert_ne!(
            sigs_before["app"], sigs_after["app"],
            "dependent must see a new signature when dep source (matched only \
             by a [tasks.*] inputs override) changes"
        );
    }

    #[test]
    fn sig_to_hex_is_64_lowercase_hex() {
        let sig: DishSig = [0xab; 32];
        let hex = sig_to_hex(&sig);
        assert_eq!(hex.len(), 64);
        assert!(hex
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
        assert_eq!(&hex[..4], "abab");
    }

    #[test]
    fn deps_for_key_is_empty_when_force_independent() {
        let tmp = two_dish_fixture();
        std::fs::write(
            tmp.path().join("app/dish.toml"),
            r#"name = "app"
depends_on = ["lib"]
force_independent = true"#,
        )
        .unwrap();
        let ws = Workspace::load(tmp.path()).unwrap();
        let graph = build_graph(&ws, "prod").unwrap();
        let reg = AdapterRegistry::builtin();
        let sigs = compute_sigs(&ws, &graph, &reg).unwrap();
        let app = &ws.dishes_by_name["app"];
        let deps = deps_for_key(&app.config, &sigs);
        assert!(deps.is_empty());
    }
}
