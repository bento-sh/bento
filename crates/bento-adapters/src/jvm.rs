//! Shared primitives for the JVM adapters (Maven, Gradle).
//!
//! Both ship a checked-in wrapper script that pins the build tool's own
//! version, and both put it at the *root* of a multi-module build only —
//! submodules inherit it. So the invocation a dish needs depends on how
//! deep it sits, which is what [`wrapper_invocation`] works out.

use std::path::Path;

/// Relative command for `dir`: the wrapper script when this dir or any
/// ancestor up to the repo root ships one (`./mvnw`, `../gradlew`,
/// `../../gradlew`, …), else `fallback` — the system tool on PATH.
///
/// The ancestor walk stops at `.git` / `bento.toml`, inclusive, so we
/// never pick up a wrapper from outside the workspace.
pub fn wrapper_invocation(dir: &Path, wrapper: &str, fallback: &str) -> String {
    let mut current = Some(dir);
    let mut depth = 0usize;
    while let Some(d) = current {
        if d.join(wrapper).is_file() {
            return if depth == 0 {
                format!("./{wrapper}")
            } else {
                format!("{}{wrapper}", "../".repeat(depth))
            };
        }
        if d.join(".git").exists() || d.join("bento.toml").is_file() {
            break;
        }
        depth += 1;
        current = d.parent();
    }
    fallback.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_wrapper_in_the_dish_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("mvnw"), "#!/bin/sh\n").unwrap();
        assert_eq!(wrapper_invocation(tmp.path(), "mvnw", "mvn"), "./mvnw");
    }

    #[test]
    fn walks_up_to_a_root_wrapper() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("mvnw"), "#!/bin/sh\n").unwrap();
        std::fs::write(tmp.path().join("bento.toml"), "").unwrap();
        let sub = tmp.path().join("services/scoring");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(wrapper_invocation(&sub, "mvnw", "mvn"), "../../mvnw");
    }

    #[test]
    fn stops_at_the_repo_root() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("bento.toml"), "").unwrap();
        let sub = tmp.path().join("app");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(wrapper_invocation(&sub, "mvnw", "mvn"), "mvn");
    }
}
