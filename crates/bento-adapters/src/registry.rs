//! [`AdapterRegistry`] — the lookup for built-in and (later) plugin-supplied
//! language adapters.

use std::path::Path;

use crate::adapter::LanguageAdapter;
use crate::bun::BunAdapter;
use crate::cargo::CargoAdapter;
use crate::deno::DenoAdapter;
use crate::go::GoAdapter;
use crate::gradle::GradleAdapter;
use crate::maven::MavenAdapter;
use crate::node_npm::NodeNpmAdapter;
use crate::php::PhpAdapter;
use crate::pnpm::PnpmAdapter;
use crate::python::PythonAdapter;
use crate::python_uv::PythonUvAdapter;
use crate::ruby::RubyAdapter;
use crate::yarn::YarnAdapter;

pub struct AdapterRegistry {
    adapters: Vec<Box<dyn LanguageAdapter>>,
}

impl AdapterRegistry {
    /// Empty registry — tests and specialised configurations.
    pub fn empty() -> Self {
        Self {
            adapters: Vec::new(),
        }
    }

    /// Registry populated with every built-in adapter.
    ///
    /// Order matters for auto-detection: the registry walks this list
    /// and returns the first adapter whose `detect()` fires. Putting
    /// more-specific lockfiles (pnpm, yarn) before npm avoids the case
    /// where a partially-migrated repo with *both* lockfiles gets
    /// classified as npm.
    pub fn builtin() -> Self {
        Self {
            adapters: vec![
                Box::new(GoAdapter),
                Box::new(CargoAdapter),
                // python-uv must be tried before python so a dish with
                // both `uv.lock` and `pyproject.toml` lands on the uv
                // path rather than the pip fallback (which fails on
                // PEP-668 hosts against the system interpreter).
                Box::new(PythonUvAdapter),
                Box::new(PythonAdapter),
                Box::new(RubyAdapter),
                Box::new(PhpAdapter),
                Box::new(MavenAdapter),
                Box::new(GradleAdapter),
                // Deno is tried before the Node family: a project with
                // `deno.json` and nothing else is unambiguously Deno, and
                // we don't want a stray `package.json` (say, for editor
                // tooling) to steal the classification.
                Box::new(DenoAdapter),
                Box::new(BunAdapter),
                Box::new(PnpmAdapter),
                Box::new(YarnAdapter),
                Box::new(NodeNpmAdapter),
            ],
        }
    }

    /// Register an additional adapter (plugin entry point).
    pub fn register(&mut self, adapter: Box<dyn LanguageAdapter>) {
        self.adapters.push(adapter);
    }

    /// Builder-style variant of [`Self::register`] for batch registration —
    /// useful for chaining off `builtin()`. Built-ins remain first in the
    /// list, so on id collision the built-in still wins via [`Self::by_id`].
    pub fn with_plugins(
        mut self,
        plugins: impl IntoIterator<Item = Box<dyn LanguageAdapter>>,
    ) -> Self {
        for p in plugins {
            self.adapters.push(p);
        }
        self
    }

    /// Look up by stable id (matches `dish.toml`'s `language` field).
    pub fn by_id(&self, id: &str) -> Option<&dyn LanguageAdapter> {
        self.adapters
            .iter()
            .find(|a| a.id() == id)
            .map(AsRef::as_ref)
    }

    /// First adapter whose `detect` returns true for `dir`.
    pub fn detect(&self, dir: &Path) -> Option<&dyn LanguageAdapter> {
        self.adapters
            .iter()
            .find(|a| a.detect(dir))
            .map(AsRef::as_ref)
    }

    /// Ids of all registered adapters, in registration order.
    pub fn ids(&self) -> Vec<String> {
        self.adapters.iter().map(|a| a.id().to_string()).collect()
    }
}

impl Default for AdapterRegistry {
    fn default() -> Self {
        Self::builtin()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_contains_all_expected_adapters() {
        let reg = AdapterRegistry::builtin();
        let ids = reg.ids();
        for want in [
            "go",
            "cargo",
            "python",
            "python-uv",
            "ruby",
            "php",
            "maven",
            "gradle",
            "node-npm",
            "node-pnpm",
            "node-yarn",
            "bun",
            "deno",
        ] {
            assert!(
                ids.iter().any(|id| id == want),
                "missing builtin adapter '{want}'"
            );
            assert!(reg.by_id(want).is_some());
        }
    }

    #[test]
    fn pnpm_wins_over_npm_when_both_lockfiles_present() {
        let reg = AdapterRegistry::builtin();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
        std::fs::write(tmp.path().join("pnpm-lock.yaml"), "").unwrap();
        std::fs::write(tmp.path().join("package-lock.json"), "{}").unwrap();
        let adapter = reg.detect(tmp.path()).expect("should detect");
        assert_eq!(adapter.id(), "node-pnpm");
    }

    #[test]
    fn yarn_wins_over_npm_when_both_lockfiles_present() {
        let reg = AdapterRegistry::builtin();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
        std::fs::write(tmp.path().join("yarn.lock"), "").unwrap();
        std::fs::write(tmp.path().join("package-lock.json"), "{}").unwrap();
        let adapter = reg.detect(tmp.path()).expect("should detect");
        assert_eq!(adapter.id(), "node-yarn");
    }

    #[test]
    fn by_id_returns_none_for_unknown() {
        let reg = AdapterRegistry::builtin();
        assert!(reg.by_id("klingon").is_none());
    }

    #[test]
    fn detect_finds_go_in_go_project() {
        let reg = AdapterRegistry::builtin();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("go.mod"), "module x\ngo 1.22\n").unwrap();
        let adapter = reg.detect(tmp.path()).expect("adapter should detect");
        assert_eq!(adapter.id(), "go");
    }

    #[test]
    fn python_uv_wins_over_python_when_uv_lock_present() {
        let reg = AdapterRegistry::builtin();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("pyproject.toml"),
            "[project]\nname = \"x\"\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("uv.lock"), "version = 1\n").unwrap();
        let adapter = reg.detect(tmp.path()).expect("should detect");
        assert_eq!(adapter.id(), "python-uv");
    }

    #[test]
    fn python_pip_handles_pyproject_without_uv_lock() {
        let reg = AdapterRegistry::builtin();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("pyproject.toml"),
            "[project]\nname = \"x\"\n",
        )
        .unwrap();
        let adapter = reg.detect(tmp.path()).expect("should detect");
        assert_eq!(adapter.id(), "python");
    }

    #[test]
    fn detect_finds_node_npm_in_npm_project() {
        let reg = AdapterRegistry::builtin();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("package.json"), r#"{"name":"x"}"#).unwrap();
        std::fs::write(tmp.path().join("package-lock.json"), "{}").unwrap();
        let adapter = reg.detect(tmp.path()).expect("adapter should detect");
        assert_eq!(adapter.id(), "node-npm");
    }

    #[test]
    fn detect_returns_none_for_unknown_project() {
        let reg = AdapterRegistry::builtin();
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("README.md"), "# something").unwrap();
        assert!(reg.detect(tmp.path()).is_none());
    }

    #[test]
    fn register_adds_custom_adapter() {
        let mut reg = AdapterRegistry::empty();
        reg.register(Box::new(GoAdapter));
        assert_eq!(reg.ids(), vec!["go".to_string()]);
    }
}
