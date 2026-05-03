//! `bento release <spec>` — cut a version of a Cargo-workspace bento
//! repo.
//!
//! MVP scope:
//!
//! - Accepts `X.Y.Z`, `patch`, `minor`, `major`, or `prerelease`.
//! - Verifies the working tree is clean before touching anything.
//! - Bumps `[workspace.package] version` and every internal
//!   `workspace.dependencies.<crate>.version` pin in the root
//!   `Cargo.toml` via `toml_edit` (comments + formatting preserved).
//! - Refreshes `Cargo.lock` by shelling out to `cargo check --locked=false
//!   --offline=false` — kept minimal; the real work is whatever cargo
//!   wants to bump internally.
//! - Creates a commit (`chore: release vX.Y.Z`) and an annotated tag
//!   (`vX.Y.Z`) locally.
//! - Prints the next steps the caller needs to take (git push both
//!   branch + tag). **Does not auto-push** — that's a big hammer and
//!   I'd rather the first release cut on a new repo goes through a
//!   human's eyes.
//!
//! Deferred (called out in bead a1p):
//!
//! - CHANGELOG auto-generation from commit log since the last tag.
//! - Floating major / minor tags (`v0`, `v0.4`).
//! - GitHub Actions `workflow_dispatch` trigger.
//! - Adapter-driven file selection (`LanguageAdapter::release_files()`).
//!   Bento OSS is Rust-only today; when a Node or Python dish needs
//!   this, that adapter grows the method.
//! - `--json` agent output mode.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use semver::Version;
use toml_edit::DocumentMut;

/// Entry point called from `main.rs`. `version_arg` is whatever the
/// user typed after `bento release` (`"0.4.0"`, `"minor"`, etc.).
pub fn run(version_arg: &str) -> Result<i32> {
    let cwd = std::env::current_dir().context("getting current dir")?;
    let root = find_cargo_workspace_root(&cwd)?;

    ensure_clean_working_tree(&root)?;

    let cargo_path = root.join("Cargo.toml");
    if !cargo_path.is_file() {
        bail!(
            "Cargo.toml not found at workspace root {} — bento release currently only supports Rust-cargo workspaces",
            root.display()
        );
    }
    let original = std::fs::read_to_string(&cargo_path)
        .with_context(|| format!("reading {}", cargo_path.display()))?;
    let current = read_workspace_version(&original)?;
    let next = parse_bump(version_arg, &current)?;
    if next == current {
        bail!("refusing to release: requested version {next} equals current workspace version");
    }

    let new_version = next.to_string();

    // Track every path we write so the git commit stages exactly the
    // set we touched — avoids `git add -u` sweeping up anyone else's
    // concurrent edits and avoids missing files we did write.
    let mut touched: Vec<PathBuf> = Vec::new();

    // Root first — it also tells us the member paths we need to walk.
    let rewritten = rewrite_workspace_versions(&original, &new_version)?;
    std::fs::write(&cargo_path, &rewritten)
        .with_context(|| format!("writing {}", cargo_path.display()))?;
    touched.push(cargo_path.clone());

    // Then every member Cargo.toml — some may pin each other by
    // path+version outside the root `[workspace.dependencies]` table
    // (bento OSS had one such: bento-adapters → bento-adapter-noop).
    for member_rel in list_workspace_members(&original)? {
        let member_cargo = root.join(&member_rel).join("Cargo.toml");
        if !member_cargo.is_file() {
            continue;
        }
        let before = std::fs::read_to_string(&member_cargo)
            .with_context(|| format!("reading {}", member_cargo.display()))?;
        let after = rewrite_member_path_pins(&before, &new_version)?;
        if after != before {
            std::fs::write(&member_cargo, &after)
                .with_context(|| format!("writing {}", member_cargo.display()))?;
            touched.push(member_cargo);
        }
    }

    // Refresh Cargo.lock. A plain `cargo check` is enough — it walks
    // the workspace and updates the lockfile in-place.
    run_cargo_check(&root).context("refreshing Cargo.lock via cargo check")?;
    touched.push(root.join("Cargo.lock"));

    let tag = format!("v{next}");
    git_commit_and_tag(&root, &next, &tag, &touched)?;

    print_next_steps(&current, &next, &tag);
    Ok(0)
}

/// Parse the user's `version_arg` against the repo's current version.
/// Accepts a concrete `X.Y.Z` or a `patch` / `minor` / `major` bump
/// keyword.
pub(crate) fn parse_bump(arg: &str, current: &Version) -> Result<Version> {
    match arg.trim() {
        "patch" => Ok(Version::new(
            current.major,
            current.minor,
            current.patch + 1,
        )),
        "minor" => Ok(Version::new(current.major, current.minor + 1, 0)),
        "major" => Ok(Version::new(current.major + 1, 0, 0)),
        // Concrete version.
        explicit => Version::parse(explicit).with_context(|| {
            format!("invalid version spec {explicit:?}; expected X.Y.Z, patch, minor, or major")
        }),
    }
}

/// Read `[workspace.package] version` from the workspace root
/// Cargo.toml contents.
pub(crate) fn read_workspace_version(cargo_toml: &str) -> Result<Version> {
    let doc: DocumentMut = cargo_toml.parse().context("parsing Cargo.toml")?;
    let v = doc
        .get("workspace")
        .and_then(|w| w.get("package"))
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("[workspace.package] version not found in Cargo.toml"))?;
    Version::parse(v).with_context(|| format!("parsing [workspace.package] version = {v:?}"))
}

/// Rewrite every version spec that identifies this workspace — the
/// `[workspace.package] version` *and* each internal-crate pin under
/// `[workspace.dependencies]`. Preserves comments, indentation, and
/// key ordering via toml_edit.
///
/// External deps (`anyhow = "1"`) are untouched by construction: we
/// only rewrite entries under `workspace.dependencies.<name>.version`
/// whose adjacent `path = "..."` points inside the workspace (any
/// `path` is enough — external crates don't declare one there).
pub(crate) fn rewrite_workspace_versions(cargo_toml: &str, new_version: &str) -> Result<String> {
    let mut doc: DocumentMut = cargo_toml.parse().context("parsing Cargo.toml")?;

    // Top-level workspace.package.version. We mutate the Value in
    // place rather than `insert`-replacing the Item so any comment
    // that decorates the `version` key (e.g. "# bumped by bento
    // release") survives the rewrite.
    {
        let pkg = doc
            .get_mut("workspace")
            .and_then(|w| w.as_table_mut())
            .and_then(|w| w.get_mut("package"))
            .and_then(|p| p.as_table_mut())
            .ok_or_else(|| anyhow!("[workspace.package] missing from Cargo.toml"))?;
        replace_string_value(pkg, "version", new_version)
            .ok_or_else(|| anyhow!("[workspace.package].version missing or not a string"))?;
    }

    // Internal deps under [workspace.dependencies] — identified by the
    // presence of a `path` key (external crates are plain version
    // strings, so .as_inline_table() is absent or `path`-less).
    if let Some(deps) = doc
        .get_mut("workspace")
        .and_then(|w| w.as_table_mut())
        .and_then(|w| w.get_mut("dependencies"))
        .and_then(|d| d.as_table_mut())
    {
        // Collect keys first — iterating the table while mutating it
        // is a borrow-checker fight we don't need to have.
        let keys: Vec<String> = deps.iter().map(|(k, _)| k.to_string()).collect();
        for key in keys {
            let Some(item) = deps.get_mut(&key) else {
                continue;
            };
            // Plain string form (`anyhow = "1"`) is always external —
            // skip.
            if item.as_str().is_some() {
                continue;
            }
            // Inline table form (`bento-core = { path = "...", version
            // = "..." }`) — keep only if it declares a `path`, which
            // by cargo convention means intra-workspace.
            if let Some(inline) = item.as_inline_table_mut() {
                if inline.get("path").is_some() {
                    // Same in-place value mutation story: preserve any
                    // comment decoration on this particular pin.
                    if let Some(v) = inline.get_mut("version") {
                        let prefix = v.decor().prefix().cloned();
                        let suffix = v.decor().suffix().cloned();
                        *v = toml_edit::Value::from(new_version);
                        if let Some(p) = prefix {
                            v.decor_mut().set_prefix(p);
                        }
                        if let Some(s) = suffix {
                            v.decor_mut().set_suffix(s);
                        }
                    }
                }
            }
        }
    }

    Ok(doc.to_string())
}

/// Return the workspace member paths (relative to the workspace root)
/// declared in the root Cargo.toml's `[workspace] members = [...]`.
/// Used by `run` to find every Cargo.toml that might carry a sibling
/// path+version pin.
pub(crate) fn list_workspace_members(cargo_toml: &str) -> Result<Vec<String>> {
    let doc: DocumentMut = cargo_toml.parse().context("parsing Cargo.toml")?;
    let arr = doc
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
        .ok_or_else(|| anyhow!("[workspace] members = [...] not found in Cargo.toml"))?;
    let mut out = Vec::with_capacity(arr.len());
    for entry in arr.iter() {
        if let Some(s) = entry.as_str() {
            out.push(s.to_string());
        }
    }
    Ok(out)
}

/// Rewrite a single member's Cargo.toml. Looks for `[dependencies]`,
/// `[dev-dependencies]`, and `[build-dependencies]` entries that are
/// inline tables with a `path` key — those are sibling-member pins
/// (e.g. `bento-adapter-noop = { path = "..", version = "0.3.1" }`)
/// whose `version` needs to move in lockstep with the workspace bump.
/// Non-path deps are left alone.
pub(crate) fn rewrite_member_path_pins(cargo_toml: &str, new_version: &str) -> Result<String> {
    let mut doc: DocumentMut = cargo_toml.parse().context("parsing member Cargo.toml")?;
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        let Some(table) = doc.get_mut(section).and_then(|t| t.as_table_mut()) else {
            continue;
        };
        let keys: Vec<String> = table.iter().map(|(k, _)| k.to_string()).collect();
        for key in keys {
            let Some(item) = table.get_mut(&key) else {
                continue;
            };
            if let Some(inline) = item.as_inline_table_mut() {
                if inline.get("path").is_some() {
                    if let Some(v) = inline.get_mut("version") {
                        let prefix = v.decor().prefix().cloned();
                        let suffix = v.decor().suffix().cloned();
                        *v = toml_edit::Value::from(new_version);
                        if let Some(p) = prefix {
                            v.decor_mut().set_prefix(p);
                        }
                        if let Some(s) = suffix {
                            v.decor_mut().set_suffix(s);
                        }
                    }
                }
            }
        }
    }
    Ok(doc.to_string())
}

/// Replace the string value at `table[key]` while keeping the existing
/// key-and-value decor (leading/trailing comments, whitespace, etc.).
/// Returns `None` if the key doesn't exist or doesn't currently hold a
/// string.
fn replace_string_value(table: &mut toml_edit::Table, key: &str, new_value: &str) -> Option<()> {
    let item = table.get_mut(key)?;
    let v = item.as_value_mut()?;
    v.as_str()?; // Must already be a string.
    let prefix = v.decor().prefix().cloned();
    let suffix = v.decor().suffix().cloned();
    *v = toml_edit::Value::from(new_value);
    if let Some(p) = prefix {
        v.decor_mut().set_prefix(p);
    }
    if let Some(s) = suffix {
        v.decor_mut().set_suffix(s);
    }
    Some(())
}

/// Shell out to `git status --porcelain` and bail if anything is
/// staged, modified, or untracked. We don't want a release commit
/// sweeping in unrelated changes.
fn ensure_clean_working_tree(root: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .output()
        .with_context(|| format!("running git status in {}", root.display()))?;
    if !output.status.success() {
        bail!(
            "git status failed ({})\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let body = String::from_utf8_lossy(&output.stdout);
    if !body.trim().is_empty() {
        bail!(
            "working tree not clean — commit, stash, or discard these first:\n{}",
            body
        );
    }
    Ok(())
}

fn run_cargo_check(root: &Path) -> Result<()> {
    // --workspace so every member's Cargo.lock entry gets refreshed.
    // --quiet to keep release output focused on the bump; cargo
    // already prints to stderr on failure.
    let output = Command::new("cargo")
        .args(["check", "--workspace", "--quiet"])
        .current_dir(root)
        .output()
        .with_context(|| format!("running cargo check in {}", root.display()))?;
    if !output.status.success() {
        bail!(
            "cargo check failed ({})\nstderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

fn git_commit_and_tag(
    root: &Path,
    version: &Version,
    tag: &str,
    touched: &[PathBuf],
) -> Result<()> {
    // Stage exactly the files we wrote — no `git add -u`, which would
    // sweep in unrelated edits made concurrently (unlikely but easy to
    // avoid). Paths are passed as workspace-relative so the command
    // line doesn't care about the caller's cwd.
    let mut add = Command::new("git");
    add.arg("add");
    for path in touched {
        let rel = path.strip_prefix(root).unwrap_or(path);
        add.arg(rel);
    }
    add.current_dir(root);
    let stage = add.output().context("running git add")?;
    if !stage.status.success() {
        bail!("git add failed: {}", String::from_utf8_lossy(&stage.stderr));
    }

    // Commit message deliberately terse — "chore: release vX.Y.Z" is
    // the de-facto standard for release bumps in cargo-heavy repos.
    let commit_msg = format!("chore: release v{version}");
    let commit = Command::new("git")
        .args(["commit", "-m", &commit_msg])
        .current_dir(root)
        .output()
        .context("running git commit")?;
    if !commit.status.success() {
        bail!(
            "git commit failed: {}",
            String::from_utf8_lossy(&commit.stderr)
        );
    }

    // Annotated tag, message mirroring the commit so `git show v0.4.0`
    // surfaces something useful.
    let tag_result = Command::new("git")
        .args(["tag", "-a", tag, "-m", &commit_msg])
        .current_dir(root)
        .output()
        .context("running git tag")?;
    if !tag_result.status.success() {
        bail!(
            "git tag {tag} failed: {}",
            String::from_utf8_lossy(&tag_result.stderr)
        );
    }

    Ok(())
}

fn print_next_steps(previous: &Version, next: &Version, tag: &str) {
    eprintln!();
    eprintln!("bento release: {previous} → {next} ({tag} created locally, not pushed)");
    eprintln!();
    eprintln!("Next steps:");
    eprintln!("  git push origin HEAD");
    eprintln!("  git push origin {tag}");
    eprintln!();
    eprintln!("Undo (local only, before push):");
    eprintln!("  git tag -d {tag} && git reset --hard HEAD~1");
    eprintln!();
}

/// Walk upward from `start` looking for the nearest Cargo workspace
/// root (a `Cargo.toml` with a `[workspace]` table). Distinct from
/// `bento_core::find_workspace_root`, which looks for a *bento*
/// workspace (`bento.toml`/`bentos/`) — bento OSS's own repo doesn't
/// have one, so the release verb must walk the Cargo tree directly.
fn find_cargo_workspace_root(start: &Path) -> Result<PathBuf> {
    let canonical = start
        .canonicalize()
        .with_context(|| format!("canonicalising {}", start.display()))?;
    let mut cursor: &Path = canonical.as_path();
    loop {
        let candidate = cursor.join("Cargo.toml");
        if candidate.is_file() {
            let body = std::fs::read_to_string(&candidate)
                .with_context(|| format!("reading {}", candidate.display()))?;
            if let Ok(doc) = body.parse::<DocumentMut>() {
                if doc.get("workspace").is_some() {
                    return Ok(cursor.to_path_buf());
                }
            }
        }
        match cursor.parent() {
            Some(p) => cursor = p,
            None => bail!(
                "no Cargo workspace root found walking up from {}",
                start.display()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn parse_bump_handles_keywords() {
        let c = v("0.3.1");
        assert_eq!(parse_bump("patch", &c).unwrap(), v("0.3.2"));
        assert_eq!(parse_bump("minor", &c).unwrap(), v("0.4.0"));
        assert_eq!(parse_bump("major", &c).unwrap(), v("1.0.0"));
    }

    #[test]
    fn parse_bump_accepts_literal_version() {
        let c = v("0.3.1");
        assert_eq!(parse_bump("0.4.0", &c).unwrap(), v("0.4.0"));
        assert_eq!(parse_bump("1.0.0-rc.1", &c).unwrap(), v("1.0.0-rc.1"));
    }

    #[test]
    fn parse_bump_rejects_garbage() {
        let c = v("0.3.1");
        let err = parse_bump("next-please", &c).unwrap_err();
        assert!(
            err.to_string().contains("invalid version spec"),
            "got {err}"
        );
    }

    #[test]
    fn read_workspace_version_finds_the_pin() {
        let toml = r#"
[workspace]
members = []
[workspace.package]
version = "0.3.1"
edition = "2021"
"#;
        assert_eq!(read_workspace_version(toml).unwrap(), v("0.3.1"));
    }

    #[test]
    fn rewrite_updates_workspace_package_and_internal_pins() {
        let toml = r#"
[workspace]
members = ["crates/a", "crates/b"]

[workspace.package]
version = "0.3.1"
edition = "2021"

[workspace.dependencies]
# External — must NOT be rewritten.
anyhow = "1"
serde = { version = "1", features = ["derive"] }
ureq = { version = "2", default-features = false }

# Internal — rewritten by path presence.
crate-a = { path = "crates/a", version = "0.3.1" }
crate-b = { path = "crates/b", version = "0.3.1", features = ["foo"] }
"#;
        let out = rewrite_workspace_versions(toml, "0.4.0").unwrap();
        assert!(
            out.contains("[workspace.package]") && out.contains("version = \"0.4.0\""),
            "workspace.package version not bumped:\n{out}"
        );
        assert!(out.contains("crate-a = { path = \"crates/a\", version = \"0.4.0\" }"));
        assert!(out.contains("crate-b = { path = \"crates/b\", version = \"0.4.0\""));
        // External pins untouched.
        assert!(out.contains("anyhow = \"1\""));
        assert!(out.contains("serde = { version = \"1\""));
        assert!(out.contains("ureq = { version = \"2\""));
    }

    #[test]
    fn rewrite_preserves_comments_and_ordering() {
        let toml = r#"[workspace]
members = []

[workspace.package]
# Keep this comment!
version = "0.3.1"
edition = "2021"
"#;
        let out = rewrite_workspace_versions(toml, "0.4.0").unwrap();
        assert!(
            out.contains("# Keep this comment!"),
            "comment dropped:\n{out}"
        );
        // edition still comes after version.
        let vpos = out.find("version = \"0.4.0\"").unwrap();
        let epos = out.find("edition").unwrap();
        assert!(vpos < epos, "key order changed:\n{out}");
    }

    #[test]
    fn rewrite_no_op_when_no_internal_pins() {
        let toml = r#"
[workspace]
members = []

[workspace.package]
version = "0.1.0"
"#;
        let out = rewrite_workspace_versions(toml, "0.2.0").unwrap();
        assert!(out.contains("version = \"0.2.0\""));
    }

    #[test]
    fn list_workspace_members_pulls_relative_paths() {
        let toml = r#"
[workspace]
members = [
    "crates/a",
    "crates/b",
    "examples/noop",
]
[workspace.package]
version = "0.1.0"
"#;
        assert_eq!(
            list_workspace_members(toml).unwrap(),
            vec!["crates/a", "crates/b", "examples/noop"]
        );
    }

    #[test]
    fn rewrite_member_path_pins_bumps_sibling_deps() {
        let toml = r#"
[package]
name = "bento-adapters"
version.workspace = true

[dependencies]
anyhow = "1"
serde = { version = "1", features = ["derive"] }
bento-core = { path = "../bento-core" }
bento-adapter-noop = { path = "../../examples/bento-adapter-noop", version = "0.3.1" }

[dev-dependencies]
bento-scratch = { path = "../bento-scratch", version = "0.3.1" }
tempfile = "3"
"#;
        let out = rewrite_member_path_pins(toml, "0.4.0").unwrap();
        assert!(
            out.contains(r#"bento-adapter-noop = { path = "../../examples/bento-adapter-noop", version = "0.4.0" }"#),
            "sibling pin not bumped:\n{out}"
        );
        assert!(
            out.contains(r#"bento-scratch = { path = "../bento-scratch", version = "0.4.0" }"#),
            "dev-dep sibling pin not bumped:\n{out}"
        );
        // Path-only dep (no explicit version) is left alone.
        assert!(out.contains(r#"bento-core = { path = "../bento-core" }"#));
        // External deps untouched.
        assert!(out.contains("anyhow = \"1\""));
        assert!(out.contains("serde = { version = \"1\""));
        assert!(out.contains("tempfile = \"3\""));
    }

    #[test]
    fn rewrite_member_path_pins_no_op_on_pure_leaf() {
        let toml = r#"
[package]
name = "bento-leaf"
version.workspace = true

[dependencies]
anyhow = "1"
"#;
        // No path+version pins → output byte-identical (modulo
        // toml_edit normalisation, which doesn't happen when nothing
        // changes).
        let out = rewrite_member_path_pins(toml, "0.4.0").unwrap();
        assert_eq!(out, toml);
    }
}
