//! Ruby adapter (bundler).
//!
//! - Detects: `Gemfile` at the dish root.
//! - Fingerprints: `Gemfile`, `Gemfile.lock`, `.ruby-version`, `.tool-versions`.
//! - Toolchain pin (priority): `.ruby-version` (rbenv/rvm/asdf convention) >
//!   the `ruby "x.y.z"` directive in `Gemfile` > `ruby` line in
//!   `.tool-versions` (asdf/mise).
//! - Install: `bundle install`.
//! - Default tasks: `bundle install` (build — Ruby is interpreted, install
//!   IS the build), `bundle exec rspec` or `bundle exec rake test` (test),
//!   `bundle exec rubocop` (lint).
//!
//! Ruby's lint story: rubocop is dominant. Users without it can override
//! the `lint` task in their `dish.toml`; same with test runner choice.
//! Standard convention beats configurability.
//!
//! No "build" in the compiled-language sense — `bundle install` is the
//! moral equivalent (downloads dependencies, regenerates lockfile if
//! Gemfile changed).

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::adapter::{DefaultTask, LanguageAdapter, TaskContext, ToolVersion};

pub struct RubyAdapter;

const FINGERPRINT: &[&str] = &["Gemfile", "Gemfile.lock", ".ruby-version", ".tool-versions"];

impl LanguageAdapter for RubyAdapter {
    fn id(&self) -> &str {
        "ruby"
    }

    fn detect(&self, dir: &Path) -> bool {
        dir.join("Gemfile").is_file()
    }

    fn fingerprint_files(&self) -> Vec<String> {
        FINGERPRINT.iter().map(|s| (*s).to_string()).collect()
    }

    fn derived_paths(&self) -> Vec<String> {
        // `bundle install` writes Gemfile.lock and, when `bundle config
        // set path vendor/bundle` is active, populates vendor/bundle/.
        // `.bundle/config` may also be written by `bundle config`. None
        // of these should contribute to task cache keys: they're
        // derived state, reproducible from the Gemfile + ruby version.
        vec![
            "Gemfile.lock".into(),
            ".bundle/**".into(),
            "vendor/bundle/**".into(),
        ]
    }

    fn required_toolchain(&self, dir: &Path) -> Result<Option<ToolVersion>> {
        // 1. `.ruby-version` — rbenv / rvm / asdf convention.
        if let Some(v) = read_first_nonempty_line(&dir.join(".ruby-version"))? {
            return Ok(Some(ToolVersion {
                tool: "ruby".into(),
                version: v,
            }));
        }
        // 2. `ruby "x.y.z"` directive in Gemfile.
        let gemfile = dir.join("Gemfile");
        if gemfile.is_file() {
            let raw = std::fs::read_to_string(&gemfile)
                .with_context(|| format!("reading {}", gemfile.display()))?;
            if let Some(version) = parse_gemfile_ruby_directive(&raw) {
                return Ok(Some(ToolVersion {
                    tool: "ruby".into(),
                    version,
                }));
            }
        }
        // 3. `ruby <version>` line in .tool-versions (asdf / mise).
        if let Some(version) = crate::tool_versions::read_tool_version(dir, &["ruby"])? {
            return Ok(Some(ToolVersion {
                tool: "ruby".into(),
                version,
            }));
        }
        Ok(None)
    }

    fn install(&self, ctx: &TaskContext) -> Result<()> {
        let mut cmd = Command::new("bundle");
        cmd.arg("install");
        ctx.apply_env(&mut cmd);
        crate::adapter::run_install_cmd(ctx, &mut cmd, "bundle install")
    }

    fn resolved_toolchain_fingerprint(&self) -> Option<String> {
        crate::probe::memoised("ruby", &["--version"])
    }

    fn default_tasks(&self) -> Vec<DefaultTask> {
        let inputs = vec!["**/*.rb".into(), "Gemfile".into(), "Gemfile.lock".into()];

        vec![
            DefaultTask {
                // Ruby is interpreted — `bundle install` is the closest
                // thing to "build". A user with a real build step (asset
                // precompile, native gem compile) overrides in dish.toml.
                name: "build".into(),
                run: "bundle install".into(),
                inputs: Some(inputs.clone()),
                outputs: None,
            },
            DefaultTask {
                // RSpec is dominant; minitest users can override.
                // We can't peek at the dish dir from default_tasks (no
                // ctx), so default to rspec — the bigger community.
                name: "test".into(),
                run: "bundle exec rspec".into(),
                inputs: Some({
                    let mut v = inputs.clone();
                    v.push("spec/**/*.rb".into());
                    v.push("test/**/*.rb".into());
                    v
                }),
                outputs: None,
            },
            DefaultTask {
                name: "lint".into(),
                run: "bundle exec rubocop".into(),
                inputs: Some({
                    let mut v = inputs;
                    v.push(".rubocop.yml".into());
                    v.push(".rubocop_todo.yml".into());
                    v
                }),
                outputs: None,
            },
        ]
    }
}

fn read_first_nonempty_line(path: &Path) -> Result<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string))
}

/// Parse the `ruby "x.y.z"` directive from a Gemfile. Accepts single or
/// double quotes; ignores `ruby_version: ...` Ruby-method-call syntax
/// and engine-specific forms (`ruby "3.2", engine: "jruby", ...`) — we
/// just want the version literal.
fn parse_gemfile_ruby_directive(content: &str) -> Option<String> {
    for raw_line in content.lines() {
        let line = raw_line
            .split_once('#')
            .map(|(code, _)| code)
            .unwrap_or(raw_line)
            .trim();
        // Match: `ruby '3.2.2'`, `ruby "3.2.2"`, with optional trailing
        // `, engine: ...`. Reject lines starting with `ruby_version`,
        // `ruby_engine`, etc.
        let Some(rest) = line.strip_prefix("ruby ") else {
            continue;
        };
        let rest = rest.trim();
        let (open, close) = if rest.starts_with('"') {
            ('"', '"')
        } else if rest.starts_with('\'') {
            ('\'', '\'')
        } else {
            continue;
        };
        let stripped = rest.strip_prefix(open)?;
        let end = stripped.find(close)?;
        let version = &stripped[..end];
        if version.is_empty() {
            continue;
        }
        return Some(version.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            std::fs::write(dir.path().join(name), content).unwrap();
        }
        dir
    }

    #[test]
    fn id_and_fingerprint() {
        let a = RubyAdapter;
        assert_eq!(a.id(), "ruby");
        let fp = a.fingerprint_files();
        for f in ["Gemfile", "Gemfile.lock", ".ruby-version"] {
            assert!(fp.iter().any(|s| s == f));
        }
    }

    #[test]
    fn detect_finds_gemfile() {
        let tmp = tmp_with(&[("Gemfile", "source 'https://rubygems.org'\n")]);
        assert!(RubyAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_rejects_non_ruby() {
        let tmp = tmp_with(&[("package.json", "{}")]);
        assert!(!RubyAdapter.detect(tmp.path()));
    }

    #[test]
    fn toolchain_prefers_dot_ruby_version() {
        let tmp = tmp_with(&[
            (".ruby-version", "3.2.2\n"),
            ("Gemfile", "ruby '3.0.0'\n"),
            (".tool-versions", "ruby 3.1.4\n"),
        ]);
        let v = RubyAdapter.required_toolchain(tmp.path()).unwrap().unwrap();
        assert_eq!(v.tool, "ruby");
        assert_eq!(v.version, "3.2.2");
    }

    #[test]
    fn toolchain_falls_back_to_gemfile_ruby_directive() {
        let tmp = tmp_with(&[("Gemfile", "source 'https://rubygems.org'\nruby '3.2.2'\n")]);
        let v = RubyAdapter.required_toolchain(tmp.path()).unwrap().unwrap();
        assert_eq!(v.version, "3.2.2");
    }

    #[test]
    fn toolchain_falls_back_to_tool_versions() {
        let tmp = tmp_with(&[
            ("Gemfile", "source 'https://rubygems.org'\n"),
            (".tool-versions", "nodejs 22.1.0\nruby 3.2.2\npython 3.12\n"),
        ]);
        let v = RubyAdapter.required_toolchain(tmp.path()).unwrap().unwrap();
        assert_eq!(v.version, "3.2.2");
    }

    #[test]
    fn toolchain_returns_none_when_unpinned() {
        let tmp = tmp_with(&[("Gemfile", "source 'https://rubygems.org'\n")]);
        assert!(RubyAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .is_none());
    }

    #[test]
    fn parse_gemfile_double_quoted() {
        assert_eq!(
            parse_gemfile_ruby_directive("ruby \"3.2.2\""),
            Some("3.2.2".into())
        );
    }

    #[test]
    fn parse_gemfile_single_quoted() {
        assert_eq!(
            parse_gemfile_ruby_directive("ruby '3.2.2'"),
            Some("3.2.2".into())
        );
    }

    #[test]
    fn parse_gemfile_with_engine_suffix() {
        // Real Gemfiles do `ruby "3.2", engine: "jruby", engine_version: ...`
        // We just want the leading version literal.
        assert_eq!(
            parse_gemfile_ruby_directive("ruby \"3.2\", engine: \"jruby\""),
            Some("3.2".into())
        );
    }

    #[test]
    fn parse_gemfile_ignores_inline_comment() {
        assert_eq!(
            parse_gemfile_ruby_directive("ruby '3.2.2' # pinned for CI"),
            Some("3.2.2".into())
        );
    }

    #[test]
    fn parse_gemfile_ignores_commented_directive() {
        assert_eq!(
            parse_gemfile_ruby_directive("# ruby '2.7'\nruby '3.2.2'"),
            Some("3.2.2".into())
        );
    }

    #[test]
    fn parse_gemfile_skips_unrelated_lines() {
        assert_eq!(
            parse_gemfile_ruby_directive("source 'https://rubygems.org'"),
            None
        );
        assert_eq!(parse_gemfile_ruby_directive("gem 'rails', '~> 7.0'"), None);
        // ruby_version is a different directive — ignore.
        assert_eq!(parse_gemfile_ruby_directive("ruby_version '3.2.2'"), None);
    }

    #[test]
    fn default_tasks_use_bundler_and_rubocop() {
        let tasks = RubyAdapter.default_tasks();
        let names: Vec<_> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["build", "test", "lint"]);
        assert_eq!(tasks[0].run, "bundle install");
        assert_eq!(tasks[1].run, "bundle exec rspec");
        assert_eq!(tasks[2].run, "bundle exec rubocop");
    }
}
