//! Maven adapter (JVM).
//!
//! - Detects: `pom.xml` at the dish root.
//! - Fingerprints: `pom.xml` (Maven has no separate lockfile — pom.xml
//!   IS the lockfile-equivalent because dependency versions are pinned
//!   inline).
//! - Toolchain pin (priority): `maven.compiler.release` > `maven.compiler.target`
//!   > `maven.compiler.source` from `pom.xml`'s `<properties>` block.
//! - Install: `mvn dependency:resolve`.
//! - Default tasks: `mvn package -DskipTests` (build), `mvn test` (test),
//!   `mvn verify -DskipTests` (lint — runs configured plugins like
//!   checkstyle, spotbugs, enforcer).
//!
//! XML parsing: regex on the three property tags. Real users with
//! exotic pom configurations (build profiles, parent inheritance,
//! plugin-config compiler version) override via `[toolchain]` in
//! dish.toml.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::adapter::{DefaultTask, LanguageAdapter, TaskContext, ToolVersion};

pub struct MavenAdapter;

const FINGERPRINT: &[&str] = &["pom.xml", ".java-version", ".sdkmanrc", ".tool-versions"];

impl LanguageAdapter for MavenAdapter {
    fn id(&self) -> &str {
        "maven"
    }

    fn detect(&self, dir: &Path) -> bool {
        dir.join("pom.xml").is_file()
    }

    fn fingerprint_files(&self) -> Vec<String> {
        FINGERPRINT.iter().map(|s| (*s).to_string()).collect()
    }

    fn required_toolchain(&self, dir: &Path) -> Result<Option<ToolVersion>> {
        // 1. `.java-version` — jenv / asdf-java fall-back convention.
        if let Some(v) = read_first_nonempty_line(&dir.join(".java-version"))? {
            return Ok(Some(ToolVersion {
                tool: "java".into(),
                version: v,
            }));
        }
        // 2. `.sdkmanrc` — sdkman convention.
        if let Some(v) = parse_sdkmanrc_java(&dir.join(".sdkmanrc"))? {
            return Ok(Some(ToolVersion {
                tool: "java".into(),
                version: v,
            }));
        }
        // 3. pom.xml maven.compiler.{release,target,source}.
        let pom = dir.join("pom.xml");
        if pom.is_file() {
            let raw = std::fs::read_to_string(&pom)
                .with_context(|| format!("reading {}", pom.display()))?;
            if let Some(version) = parse_compiler_version(&raw) {
                return Ok(Some(ToolVersion {
                    tool: "java".into(),
                    version,
                }));
            }
        }
        // 4. .tool-versions (asdf/mise).
        if let Some(v) = crate::tool_versions::read_tool_version(dir, &["java"])? {
            return Ok(Some(ToolVersion {
                tool: "java".into(),
                version: v,
            }));
        }
        Ok(None)
    }

    fn install(&self, ctx: &TaskContext) -> Result<()> {
        let mut cmd = Command::new("mvn");
        cmd.args(["dependency:resolve", "-q"]);
        ctx.apply_env(&mut cmd);
        crate::adapter::run_install_cmd(ctx, &mut cmd, "mvn dependency:resolve")
    }

    fn resolved_toolchain_fingerprint(&self) -> Option<String> {
        // `java -version` writes to stderr, not stdout. Probe both via a
        // wrapper that returns whichever has content.
        java_version()
    }

    fn default_tasks(&self) -> Vec<DefaultTask> {
        let inputs = vec!["src/**".into(), "pom.xml".into()];

        vec![
            DefaultTask {
                name: "build".into(),
                run: "mvn package -DskipTests".into(),
                inputs: Some(inputs.clone()),
                outputs: Some(vec!["target/*.jar".into(), "target/*.war".into()]),
            },
            DefaultTask {
                name: "test".into(),
                run: "mvn test".into(),
                inputs: Some({
                    let mut v = inputs.clone();
                    v.push("src/test/**".into());
                    v
                }),
                outputs: None,
            },
            DefaultTask {
                // `verify` runs configured quality plugins (checkstyle,
                // spotbugs, enforcer, etc.) without re-running tests.
                name: "lint".into(),
                run: "mvn verify -DskipTests".into(),
                inputs: Some({
                    let mut v = inputs;
                    v.push("checkstyle.xml".into());
                    v.push("spotbugs-exclude.xml".into());
                    v
                }),
                outputs: None,
            },
        ]
    }
}

/// Pull the JVM compiler version out of a pom.xml. Walks the `<properties>`
/// block in priority order: `maven.compiler.release` (preferred — includes
/// API restrictions) > `maven.compiler.target` > `maven.compiler.source`.
///
/// Regex-based: a real XML parser would handle this with less surface area
/// for edge cases, but Maven properties are deeply conventional enough
/// that simple matching is fine for v1. Complex setups (parent POM
/// inheritance, build-profile overrides, plugin-config versions) override
/// via `[toolchain]` in their `dish.toml`.
fn parse_compiler_version(content: &str) -> Option<String> {
    for tag in [
        "maven.compiler.release",
        "maven.compiler.target",
        "maven.compiler.source",
    ] {
        if let Some(v) = extract_property(content, tag) {
            return Some(v);
        }
    }
    None
}

fn extract_property(content: &str, name: &str) -> Option<String> {
    // Match `<name>value</name>` with optional whitespace inside.
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = content.find(&open)?;
    let after = &content[start + open.len()..];
    let end = after.find(&close)?;
    let value = after[..end].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
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

/// Parse a Java version out of `.sdkmanrc`. Format is INI-ish:
/// `java=21.0.2-tem` per line, comment-prefixed with `#`.
fn parse_sdkmanrc_java(path: &Path) -> Result<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    for raw_line in raw.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() == "java" {
            let v = value.trim();
            if !v.is_empty() {
                return Ok(Some(v.to_string()));
            }
        }
    }
    Ok(None)
}

/// `java -version` is famously written to stderr. Probe both streams.
fn java_version() -> Option<String> {
    let output = std::process::Command::new("java")
        .arg("-version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    // Prefer stderr (the canonical path); fall back to stdout in case a
    // wrapper inverted them.
    let mut text = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if text.is_empty() {
        text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    }
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
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

    fn pom(properties: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<project>
  <groupId>com.example</groupId>
  <artifactId>app</artifactId>
  <version>1.0.0</version>
  <properties>
{properties}
  </properties>
</project>
"#
        )
    }

    #[test]
    fn id_and_fingerprint() {
        let a = MavenAdapter;
        assert_eq!(a.id(), "maven");
        let fp = a.fingerprint_files();
        assert!(fp.iter().any(|s| s == "pom.xml"));
    }

    #[test]
    fn detect_finds_pom_xml() {
        let tmp = tmp_with(&[("pom.xml", &pom(""))]);
        assert!(MavenAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_rejects_non_maven() {
        let tmp = tmp_with(&[("build.gradle", "")]);
        assert!(!MavenAdapter.detect(tmp.path()));
    }

    #[test]
    fn toolchain_prefers_release_over_target_and_source() {
        let body = pom("    <maven.compiler.release>21</maven.compiler.release>\n\
             \x20\x20\x20\x20<maven.compiler.target>17</maven.compiler.target>\n\
             \x20\x20\x20\x20<maven.compiler.source>11</maven.compiler.source>");
        let tmp = tmp_with(&[("pom.xml", &body)]);
        let v = MavenAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.tool, "java");
        assert_eq!(v.version, "21");
    }

    #[test]
    fn toolchain_falls_back_to_target_when_no_release() {
        let body = pom("    <maven.compiler.target>17</maven.compiler.target>\n\
             \x20\x20\x20\x20<maven.compiler.source>11</maven.compiler.source>");
        let tmp = tmp_with(&[("pom.xml", &body)]);
        let v = MavenAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.version, "17");
    }

    #[test]
    fn toolchain_falls_back_to_source_when_only_source() {
        let body = pom("    <maven.compiler.source>11</maven.compiler.source>");
        let tmp = tmp_with(&[("pom.xml", &body)]);
        let v = MavenAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.version, "11");
    }

    #[test]
    fn toolchain_returns_none_when_no_compiler_property() {
        let body = pom("    <jacoco.version>0.8.11</jacoco.version>");
        let tmp = tmp_with(&[("pom.xml", &body)]);
        assert!(MavenAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .is_none());
    }

    #[test]
    fn toolchain_handles_whitespace_around_value() {
        let body = pom("    <maven.compiler.release>\n      21\n    </maven.compiler.release>");
        let tmp = tmp_with(&[("pom.xml", &body)]);
        let v = MavenAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.version, "21");
    }

    #[test]
    fn extract_property_handles_missing_close_tag() {
        // Malformed XML — extract_property returns None rather than panic.
        assert_eq!(
            extract_property("<maven.compiler.release>21", "maven.compiler.release"),
            None
        );
    }

    #[test]
    fn default_tasks_use_mvn_lifecycle() {
        let tasks = MavenAdapter.default_tasks();
        let names: Vec<_> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["build", "test", "lint"]);
        assert_eq!(tasks[0].run, "mvn package -DskipTests");
        assert_eq!(tasks[1].run, "mvn test");
        assert_eq!(tasks[2].run, "mvn verify -DskipTests");
    }
}
