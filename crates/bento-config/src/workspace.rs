//! Workspace discovery: walk a repo, parse every config, return a validated
//! in-memory model.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::ConfigError;
use crate::schema::{parse_bento, parse_dish, parse_repo, BentoConfig, DishConfig, RepoConfig};

/// A loaded bento with its source path.
#[derive(Debug, Clone)]
pub struct LoadedBento {
    pub config: BentoConfig,
    pub source: PathBuf,
}

/// A loaded dish with its source directory (parent of `dish.toml`).
#[derive(Debug, Clone)]
pub struct LoadedDish {
    pub config: DishConfig,
    /// Directory containing `dish.toml`.
    pub dir: PathBuf,
    /// Relative path from the workspace root (stable key across machines).
    pub rel: PathBuf,
}

/// A fully loaded workspace — repo config, all bentos, all dishes, and
/// validated cross-references.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub root: PathBuf,
    pub repo: RepoConfig,
    /// Bentos keyed by name.
    pub bentos: BTreeMap<String, LoadedBento>,
    /// Dishes keyed by their relative path from `root` (e.g. `"apps/api"`).
    pub dishes_by_path: BTreeMap<PathBuf, LoadedDish>,
    /// Dishes keyed by `DishConfig.name`.
    pub dishes_by_name: BTreeMap<String, LoadedDish>,
}

impl Workspace {
    /// Discover and load a workspace rooted at `root`.
    ///
    /// - Reads `<root>/bento.toml` if present (otherwise uses defaults).
    /// - Loads every `<root>/bentos/*.toml`.
    /// - Loads each dish referenced by a bento's `dishes` list.
    /// - Validates: unique bento names, unique dish names, every referenced
    ///   dish path has a `dish.toml`.
    pub fn load(root: &Path) -> Result<Self, ConfigError> {
        let repo = load_repo_config(root)?;
        let bentos = load_bentos(root)?;
        let DishIndex { by_path, by_name } = load_dishes(root, &bentos)?;

        Ok(Workspace {
            root: root.to_path_buf(),
            repo,
            bentos,
            dishes_by_path: by_path,
            dishes_by_name: by_name,
        })
    }
}

fn load_repo_config(root: &Path) -> Result<RepoConfig, ConfigError> {
    let path = root.join("bento.toml");
    if path.exists() {
        parse_repo(&path)
    } else {
        Ok(RepoConfig::default())
    }
}

fn load_bentos(root: &Path) -> Result<BTreeMap<String, LoadedBento>, ConfigError> {
    let mut out = BTreeMap::new();
    let dir = root.join("bentos");
    if !dir.exists() {
        return Ok(out);
    }

    let entries = std::fs::read_dir(&dir).map_err(|e| ConfigError::Read {
        path: dir.clone(),
        source: e,
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| ConfigError::Read {
            path: dir.clone(),
            source: e,
        })?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }

        let config = parse_bento(&path)?;
        let loaded = LoadedBento {
            config: config.clone(),
            source: path.clone(),
        };
        if let Some(prev) = out.insert(config.name.clone(), loaded) {
            return Err(ConfigError::Duplicate {
                kind: "bento",
                name: config.name,
                path_a: prev.source,
                path_b: path,
            });
        }
    }
    Ok(out)
}

struct DishIndex {
    by_path: BTreeMap<PathBuf, LoadedDish>,
    by_name: BTreeMap<String, LoadedDish>,
}

fn load_dishes(
    root: &Path,
    bentos: &BTreeMap<String, LoadedBento>,
) -> Result<DishIndex, ConfigError> {
    let mut by_path: BTreeMap<PathBuf, LoadedDish> = BTreeMap::new();
    let mut by_name: BTreeMap<String, LoadedDish> = BTreeMap::new();

    for bento in bentos.values() {
        for dish_ref in &bento.config.dishes {
            let rel = PathBuf::from(dish_ref);
            if by_path.contains_key(&rel) {
                continue; // same dish shared across bentos — load once
            }

            let dir = root.join(&rel);
            let toml_path = dir.join("dish.toml");
            if !toml_path.exists() {
                return Err(ConfigError::DanglingDishRef {
                    bento: bento.config.name.clone(),
                    dish_path: rel.clone(),
                });
            }

            let config = parse_dish(&toml_path)?;
            let loaded = LoadedDish {
                config: config.clone(),
                dir,
                rel: rel.clone(),
            };

            if let Some(prev) = by_name.insert(config.name.clone(), loaded.clone()) {
                return Err(ConfigError::Duplicate {
                    kind: "dish",
                    name: config.name,
                    path_a: prev.dir.join("dish.toml"),
                    path_b: toml_path,
                });
            }
            by_path.insert(rel, loaded);
        }
    }

    Ok(DishIndex { by_path, by_name })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a small two-dish sample workspace in a tempdir and return it.
    fn two_dish_fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(
            root.join("bento.toml"),
            r#"
            [defaults]
            parallelism = 4
            [cache]
            local = true
            gha = "auto"
            "#,
        )
        .unwrap();

        std::fs::create_dir(root.join("bentos")).unwrap();
        std::fs::write(
            root.join("bentos/prod.toml"),
            r#"
            name = "prod"
            dishes = ["apps/api", "apps/web"]
            "#,
        )
        .unwrap();

        let api = root.join("apps/api");
        std::fs::create_dir_all(&api).unwrap();
        std::fs::write(
            api.join("dish.toml"),
            r#"
            name = "sample-api"
            language = "go"

            [tasks.build]
            run = "go build -o bin/api ./cmd/api"
            "#,
        )
        .unwrap();

        let web = root.join("apps/web");
        std::fs::create_dir_all(&web).unwrap();
        std::fs::write(
            web.join("dish.toml"),
            r#"
            name = "sample-web"
            language = "node"
            package_manager = "npm"
            depends_on = ["sample-api"]

            [tasks.build]
            run = "npm run build"
            "#,
        )
        .unwrap();

        tmp
    }

    #[test]
    fn loads_two_dish_workspace() {
        let tmp = two_dish_fixture();
        let ws = Workspace::load(tmp.path()).unwrap();

        assert_eq!(ws.repo.defaults.parallelism, 4);
        assert_eq!(ws.bentos.len(), 1);
        assert_eq!(ws.bentos["prod"].config.dishes.len(), 2);
        assert_eq!(ws.dishes_by_path.len(), 2);
        assert_eq!(ws.dishes_by_name.len(), 2);

        let api = &ws.dishes_by_name["sample-api"];
        assert_eq!(api.config.language.as_deref(), Some("go"));
        assert_eq!(api.rel, PathBuf::from("apps/api"));

        let web = &ws.dishes_by_name["sample-web"];
        assert_eq!(web.config.depends_on, vec!["sample-api"]);
    }

    #[test]
    fn workspace_without_bento_toml_uses_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("bentos")).unwrap();
        std::fs::write(
            tmp.path().join("bentos/empty.toml"),
            r#"
            name = "empty"
            dishes = ["apps/only"]
            "#,
        )
        .unwrap();
        let only = tmp.path().join("apps/only");
        std::fs::create_dir_all(&only).unwrap();
        std::fs::write(only.join("dish.toml"), r#"name = "only""#).unwrap();

        let ws = Workspace::load(tmp.path()).unwrap();
        assert_eq!(
            ws.repo.defaults.parallelism,
            RepoConfig::default().defaults.parallelism
        );
        assert!(ws.repo.cache.local);
    }

    #[test]
    fn dangling_dish_reference_is_caught() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("bentos")).unwrap();
        std::fs::write(
            tmp.path().join("bentos/prod.toml"),
            r#"
            name = "prod"
            dishes = ["apps/nowhere"]
            "#,
        )
        .unwrap();

        let err = Workspace::load(tmp.path()).unwrap_err();
        match err {
            ConfigError::DanglingDishRef { bento, dish_path } => {
                assert_eq!(bento, "prod");
                assert_eq!(dish_path, PathBuf::from("apps/nowhere"));
            }
            other => panic!("expected DanglingDishRef, got: {other:?}"),
        }
    }

    #[test]
    fn duplicate_bento_name_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("bentos")).unwrap();
        std::fs::write(
            tmp.path().join("bentos/a.toml"),
            r#"name = "prod"
dishes = ["apps/api"]"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("bentos/b.toml"),
            r#"name = "prod"
dishes = ["apps/api"]"#,
        )
        .unwrap();

        let api = tmp.path().join("apps/api");
        std::fs::create_dir_all(&api).unwrap();
        std::fs::write(api.join("dish.toml"), r#"name = "api""#).unwrap();

        let err = Workspace::load(tmp.path()).unwrap_err();
        assert!(matches!(err, ConfigError::Duplicate { kind: "bento", .. }));
    }

    #[test]
    fn duplicate_dish_name_across_bentos_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("bentos")).unwrap();
        std::fs::write(
            tmp.path().join("bentos/prod.toml"),
            r#"name = "prod"
dishes = ["apps/a", "apps/b"]"#,
        )
        .unwrap();

        for (subdir, _) in &[("apps/a", ()), ("apps/b", ())] {
            let d = tmp.path().join(subdir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("dish.toml"), r#"name = "samename""#).unwrap();
        }

        let err = Workspace::load(tmp.path()).unwrap_err();
        assert!(matches!(err, ConfigError::Duplicate { kind: "dish", .. }));
    }

    #[test]
    fn shared_dish_across_bentos_loads_once() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("bentos")).unwrap();
        std::fs::write(
            tmp.path().join("bentos/staging.toml"),
            r#"name = "staging"
dishes = ["apps/api"]"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("bentos/prod.toml"),
            r#"name = "prod"
dishes = ["apps/api"]"#,
        )
        .unwrap();

        let api = tmp.path().join("apps/api");
        std::fs::create_dir_all(&api).unwrap();
        std::fs::write(api.join("dish.toml"), r#"name = "api""#).unwrap();

        let ws = Workspace::load(tmp.path()).unwrap();
        assert_eq!(ws.bentos.len(), 2);
        assert_eq!(ws.dishes_by_name.len(), 1);
    }
}
