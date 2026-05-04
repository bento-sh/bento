//! Toolchain resolution — for a given dish, decide which version of which
//! tool should run its tasks.
//!
//! Resolution priority (highest first):
//!
//! 1. **Dish** — `dish.toml`'s `[toolchain]` block (`go = "1.22.3"`, …).
//! 2. **Repo** — `bento.toml`'s `[toolchain]` block.
//! 3. **Adapter** — whatever the language adapter parses from the project
//!    itself (e.g. the `go <ver>` directive in `go.mod`, `engines.node`
//!    in `package.json`).
//! 4. **System** — fall through to the binary already on `PATH`.
//!
//! Both the dish and repo blocks support `use_system = true` as an opt-out:
//! if either says so, the entire bento-managed toolchain layer is skipped
//! and the host's `PATH` is trusted.

use std::path::Path;

use anyhow::Result;

use bento_adapters::LanguageAdapter;
use bento_config::{DishConfig, RepoConfig};

/// Where the resolved version came from. Surfaced in `bento why` and
/// `bento toolchain list` so the user can trace a surprise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionSource {
    /// `dish.toml` `[toolchain]` block.
    Dish,
    /// `bento.toml` `[toolchain]` block.
    Repo,
    /// Adapter-derived (e.g. `go.mod`'s `go` directive).
    Adapter,
    /// No bento-managed pin; use whatever the host's `PATH` resolves.
    System,
}

impl ResolutionSource {
    pub fn label(self) -> &'static str {
        match self {
            ResolutionSource::Dish => "dish.toml",
            ResolutionSource::Repo => "bento.toml",
            ResolutionSource::Adapter => "adapter",
            ResolutionSource::System => "system PATH",
        }
    }
}

/// The end result of resolving one tool for one dish. `None` for `version`
/// means "use whatever's on the system".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub tool: String,
    pub version: Option<String>,
    pub source: ResolutionSource,
}

impl Resolution {
    pub fn pinned(
        tool: impl Into<String>,
        version: impl Into<String>,
        source: ResolutionSource,
    ) -> Self {
        Self {
            tool: tool.into(),
            version: Some(version.into()),
            source,
        }
    }

    pub fn system(tool: impl Into<String>) -> Self {
        Self {
            tool: tool.into(),
            version: None,
            source: ResolutionSource::System,
        }
    }

    pub fn is_pinned(&self) -> bool {
        self.version.is_some()
    }
}

/// Stateless function: figure out which toolchain version (if any) governs
/// a single dish for the tool the adapter manages.
///
/// `dish_dir` is needed because the adapter may parse files inside the
/// dish (e.g. read the `go` directive from `<dish>/go.mod`).
pub struct Resolver;

impl Resolver {
    /// Resolve toolchain for `dish` against the workspace `repo` config and
    /// its detected `adapter`.
    ///
    /// Returns `None` only when `use_system = true` was set at *either*
    /// scope (the user explicitly opted out of bento-managed toolchains
    /// for this scope) — in that case the caller should not install or
    /// PATH-inject anything.
    pub fn resolve(
        dish_dir: &Path,
        dish: &DishConfig,
        repo: &RepoConfig,
        adapter: &dyn LanguageAdapter,
    ) -> Result<Option<Resolution>> {
        // Hard opt-out: dish first, then repo. Either kills the layer.
        if dish.toolchain.as_ref().is_some_and(|t| t.use_system) {
            return Ok(None);
        }
        if repo.toolchain.use_system {
            return Ok(None);
        }

        // Tool name comes from the adapter — "go", "node", etc.
        let tool = primary_tool_name(adapter);

        // 1. Dish-level pin.
        if let Some(t) = &dish.toolchain {
            if let Some(v) = t.pins.get(&tool) {
                return Ok(Some(Resolution::pinned(
                    tool,
                    v.clone(),
                    ResolutionSource::Dish,
                )));
            }
        }

        // 2. Repo-level pin.
        if let Some(v) = repo.toolchain.pins.get(&tool) {
            return Ok(Some(Resolution::pinned(
                tool,
                v.clone(),
                ResolutionSource::Repo,
            )));
        }

        // 3. Adapter-derived (parsed from project files).
        if let Some(version) = adapter.required_toolchain(dish_dir)? {
            return Ok(Some(Resolution::pinned(
                version.tool.clone(),
                version.version.clone(),
                ResolutionSource::Adapter,
            )));
        }

        // 4. System fallback.
        Ok(Some(Resolution::system(tool)))
    }
}

/// Map an adapter id to its canonical tool name. Adapters can declare
/// multiple tools (e.g. node-pnpm needs both Node and pnpm), but today
/// we resolve one primary tool per adapter; the rest will land with the
/// dep-graph work.
fn primary_tool_name(adapter: &dyn LanguageAdapter) -> String {
    match adapter.id() {
        "go" => "go".to_string(),
        // npm / pnpm / yarn all run on Node — share one toolchain pin.
        "node-npm" | "node-pnpm" | "node-yarn" => "node".to_string(),
        // Bun and Deno are independently versioned runtimes — they have
        // their own pins. (The bun adapter still falls back to a Node
        // pin when `.bun-version` is absent, but that's per-dish via
        // `required_toolchain`, not a primary-tool collapse.)
        "bun" => "bun".to_string(),
        "deno" => "deno".to_string(),
        // Both python adapters share one toolchain — uv-managed Python
        // works for either pip-based or uv-based dishes.
        "python" | "python-uv" => "python".to_string(),
        // Fallback: use the adapter's own id. Lets future adapters work
        // without a registry change here.
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use bento_adapters::{DefaultTask, GoAdapter, LanguageAdapter, TaskContext, ToolVersion};
    use bento_config::{DishConfig, RepoConfig, ToolchainPin};

    fn make_repo_with(pins: &[(&str, &str)], use_system: bool) -> RepoConfig {
        let toolchain = ToolchainPin {
            use_system,
            pins: pins
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        RepoConfig {
            toolchain,
            ..Default::default()
        }
    }

    fn make_dish_with(pins: &[(&str, &str)], use_system: bool) -> DishConfig {
        let toolchain = ToolchainPin {
            use_system,
            pins: pins
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        };
        DishConfig {
            name: "test".into(),
            toolchain: Some(toolchain),
            ..Default::default()
        }
    }

    /// A minimal `LanguageAdapter` for tests so we don't need real go.mod
    /// files on disk for every assertion.
    struct StubAdapter {
        id: &'static str,
        from_project: Option<ToolVersion>,
    }

    impl LanguageAdapter for StubAdapter {
        fn id(&self) -> &str {
            self.id
        }
        fn detect(&self, _dir: &Path) -> bool {
            true
        }
        fn fingerprint_files(&self) -> Vec<String> {
            Vec::new()
        }
        fn required_toolchain(&self, _dir: &Path) -> Result<Option<ToolVersion>> {
            Ok(self.from_project.clone())
        }
        fn install(&self, _ctx: &TaskContext) -> Result<()> {
            Ok(())
        }
        fn default_tasks(&self) -> Vec<DefaultTask> {
            Vec::new()
        }
    }

    #[test]
    fn dish_pin_wins_over_everything() {
        let dish = make_dish_with(&[("go", "1.22.3")], false);
        let repo = make_repo_with(&[("go", "1.21.0")], false);
        let adapter = StubAdapter {
            id: "go",
            from_project: Some(ToolVersion {
                tool: "go".into(),
                version: "1.20".into(),
            }),
        };
        let r = Resolver::resolve(&PathBuf::from("."), &dish, &repo, &adapter)
            .unwrap()
            .unwrap();
        assert_eq!(r.tool, "go");
        assert_eq!(r.version.as_deref(), Some("1.22.3"));
        assert_eq!(r.source, ResolutionSource::Dish);
    }

    #[test]
    fn repo_pin_wins_when_dish_silent() {
        let dish = DishConfig::default();
        let repo = make_repo_with(&[("go", "1.21.0")], false);
        let adapter = StubAdapter {
            id: "go",
            from_project: Some(ToolVersion {
                tool: "go".into(),
                version: "1.20".into(),
            }),
        };
        let r = Resolver::resolve(&PathBuf::from("."), &dish, &repo, &adapter)
            .unwrap()
            .unwrap();
        assert_eq!(r.version.as_deref(), Some("1.21.0"));
        assert_eq!(r.source, ResolutionSource::Repo);
    }

    #[test]
    fn adapter_wins_when_no_pin_set() {
        let dish = DishConfig::default();
        let repo = RepoConfig::default();
        let adapter = StubAdapter {
            id: "go",
            from_project: Some(ToolVersion {
                tool: "go".into(),
                version: "1.20".into(),
            }),
        };
        let r = Resolver::resolve(&PathBuf::from("."), &dish, &repo, &adapter)
            .unwrap()
            .unwrap();
        assert_eq!(r.version.as_deref(), Some("1.20"));
        assert_eq!(r.source, ResolutionSource::Adapter);
    }

    #[test]
    fn system_fallback_when_nothing_pinned() {
        let dish = DishConfig::default();
        let repo = RepoConfig::default();
        let adapter = StubAdapter {
            id: "go",
            from_project: None,
        };
        let r = Resolver::resolve(&PathBuf::from("."), &dish, &repo, &adapter)
            .unwrap()
            .unwrap();
        assert!(r.version.is_none());
        assert_eq!(r.source, ResolutionSource::System);
    }

    #[test]
    fn dish_use_system_skips_layer_entirely() {
        let dish = make_dish_with(&[], true);
        let repo = make_repo_with(&[("go", "1.21.0")], false);
        let adapter = StubAdapter {
            id: "go",
            from_project: None,
        };
        assert!(
            Resolver::resolve(&PathBuf::from("."), &dish, &repo, &adapter)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn repo_use_system_skips_layer_entirely() {
        let dish = DishConfig::default();
        let repo = make_repo_with(&[], true);
        let adapter = StubAdapter {
            id: "go",
            from_project: Some(ToolVersion {
                tool: "go".into(),
                version: "1.20".into(),
            }),
        };
        assert!(
            Resolver::resolve(&PathBuf::from("."), &dish, &repo, &adapter)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn node_npm_adapter_resolves_node_pin() {
        // The 'node-npm' adapter should look up its primary tool name as
        // 'node' (not 'node-npm') when checking pin maps.
        let dish = DishConfig::default();
        let repo = make_repo_with(&[("node", "22.1.0")], false);
        let adapter = StubAdapter {
            id: "node-npm",
            from_project: None,
        };
        let r = Resolver::resolve(&PathBuf::from("."), &dish, &repo, &adapter)
            .unwrap()
            .unwrap();
        assert_eq!(r.tool, "node");
        assert_eq!(r.version.as_deref(), Some("22.1.0"));
        assert_eq!(r.source, ResolutionSource::Repo);
    }

    #[test]
    fn bun_adapter_resolves_bun_pin_not_node_pin() {
        // Regression: the bun adapter used to share node's pin slot, so
        // a `[toolchain] bun = "1.3.12"` repo pin was silently shadowed
        // by the node pin and never reached the installer.
        let dish = DishConfig::default();
        let repo = make_repo_with(&[("node", "20.20.0"), ("bun", "1.3.12")], false);
        let adapter = StubAdapter {
            id: "bun",
            from_project: None,
        };
        let r = Resolver::resolve(&PathBuf::from("."), &dish, &repo, &adapter)
            .unwrap()
            .unwrap();
        assert_eq!(r.tool, "bun");
        assert_eq!(r.version.as_deref(), Some("1.3.12"));
        assert_eq!(r.source, ResolutionSource::Repo);
    }

    #[test]
    fn deno_adapter_resolves_deno_pin_not_node_pin() {
        let dish = DishConfig::default();
        let repo = make_repo_with(&[("node", "20.20.0"), ("deno", "1.46.0")], false);
        let adapter = StubAdapter {
            id: "deno",
            from_project: None,
        };
        let r = Resolver::resolve(&PathBuf::from("."), &dish, &repo, &adapter)
            .unwrap()
            .unwrap();
        assert_eq!(r.tool, "deno");
        assert_eq!(r.version.as_deref(), Some("1.46.0"));
        assert_eq!(r.source, ResolutionSource::Repo);
    }

    /// Sanity: real GoAdapter against an in-memory-ish dish dir reads go.mod.
    #[test]
    fn real_go_adapter_integration() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("go.mod"),
            "module example.com/x\n\ngo 1.23\n",
        )
        .unwrap();
        let dish = DishConfig::default();
        let repo = RepoConfig::default();
        let adapter = GoAdapter;
        let r = Resolver::resolve(tmp.path(), &dish, &repo, &adapter)
            .unwrap()
            .unwrap();
        assert_eq!(r.version.as_deref(), Some("1.23"));
        assert_eq!(r.source, ResolutionSource::Adapter);
    }

    #[test]
    fn resolution_is_pinned_helper() {
        let pinned = Resolution::pinned("go", "1.22", ResolutionSource::Dish);
        assert!(pinned.is_pinned());
        let system = Resolution::system("go");
        assert!(!system.is_pinned());
    }

    #[test]
    fn resolution_source_labels_are_human_readable() {
        assert_eq!(ResolutionSource::Dish.label(), "dish.toml");
        assert_eq!(ResolutionSource::Repo.label(), "bento.toml");
        assert_eq!(ResolutionSource::Adapter.label(), "adapter");
        assert_eq!(ResolutionSource::System.label(), "system PATH");
    }

    // Surface the unused import explicitly as a compile-time canary in case
    // someone removes BTreeMap usage above.
    #[test]
    fn type_imports_compile() {
        let _: BTreeMap<String, String> = BTreeMap::new();
    }
}
