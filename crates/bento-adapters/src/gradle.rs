//! Gradle adapter (JVM).
//!
//! Trickier than Maven because Gradle is programmable: `build.gradle`
//! and `build.gradle.kts` are full Groovy / Kotlin scripts, and pinning
//! a Java version can live in any of:
//!
//! - `gradle/libs.versions.toml` `[versions] java = ...` (modern,
//!   declarative — version catalog convention)
//! - `gradle.properties` `java.toolchain.languageVersion=N` (a
//!   convention some teams adopt for non-DSL config)
//! - `JavaLanguageVersion.of(N)` inside `java { toolchain { ... } }`
//!   in `build.gradle{.kts}` (the canonical Gradle 7+ idiom)
//! - Plain `sourceCompatibility = JavaVersion.VERSION_N` /
//!   `sourceCompatibility = '17'` in older builds
//!
//! We do best-effort regex extraction across these; if your Gradle
//! setup is genuinely programmable (computed versions, parent-build
//! inheritance), declare it explicitly via `[toolchain]` in your
//! `dish.toml`.
//!
//! - Detects: any of `build.gradle`, `build.gradle.kts`, `settings.gradle`,
//!   `settings.gradle.kts` at the dish root.
//! - Install: `./gradlew dependencies` (or `gradle` if the wrapper isn't
//!   present).
//! - Default tasks use `./gradlew` — every modern Gradle project ships
//!   the wrapper, and the bootstrap-correct command is wrapper-first.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::adapter::{DefaultTask, LanguageAdapter, TaskContext, ToolVersion};

pub struct GradleAdapter;

const FINGERPRINT: &[&str] = &[
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "gradle.properties",
    "gradle/libs.versions.toml",
    "gradle/wrapper/gradle-wrapper.properties",
    ".java-version",
    ".sdkmanrc",
    ".tool-versions",
];

const DETECT_FILES: &[&str] = &[
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
];

impl LanguageAdapter for GradleAdapter {
    fn id(&self) -> &str {
        "gradle"
    }

    fn detect(&self, dir: &Path) -> bool {
        DETECT_FILES.iter().any(|f| dir.join(f).is_file())
    }

    fn fingerprint_files(&self) -> Vec<String> {
        FINGERPRINT.iter().map(|s| (*s).to_string()).collect()
    }

    fn required_toolchain(&self, dir: &Path) -> Result<Option<ToolVersion>> {
        // 1. `.java-version` — universal jenv / asdf-java convention.
        if let Some(v) = read_first_nonempty_line(&dir.join(".java-version"))? {
            return Ok(Some(java(v)));
        }
        // 2. `.sdkmanrc` — sdkman convention.
        if let Some(v) = parse_sdkmanrc_java(&dir.join(".sdkmanrc"))? {
            return Ok(Some(java(v)));
        }
        // 3. gradle/libs.versions.toml [versions] java = "..."
        if let Some(v) = parse_libs_versions_java(&dir.join("gradle/libs.versions.toml"))? {
            return Ok(Some(java(v)));
        }
        // 4. gradle.properties: `java.toolchain.languageVersion=N`.
        if let Some(v) = parse_gradle_properties_java(&dir.join("gradle.properties"))? {
            return Ok(Some(java(v)));
        }
        // 5. build.gradle{.kts}: regex for `JavaLanguageVersion.of(N)` or
        //    `sourceCompatibility = ...`. Best-effort — Gradle is a
        //    programming language and we don't run a Groovy/Kotlin
        //    interpreter.
        for file in ["build.gradle.kts", "build.gradle"] {
            if let Some(v) = parse_build_gradle_java(&dir.join(file))? {
                return Ok(Some(java(v)));
            }
        }
        // 6. .tool-versions (asdf/mise).
        if let Some(v) = crate::tool_versions::read_tool_version(dir, &["java"])? {
            return Ok(Some(java(v)));
        }
        Ok(None)
    }

    fn install(&self, ctx: &TaskContext) -> Result<()> {
        let (program, owned_program) = invocation(&ctx.dish_dir);
        let mut cmd = Command::new(program);
        cmd.args(["dependencies", "--quiet"]);
        ctx.apply_env(&mut cmd);
        crate::adapter::run_install_cmd(ctx, &mut cmd, &format!("{owned_program} dependencies"))
    }

    fn resolved_toolchain_fingerprint(&self) -> Option<String> {
        // `java -version` writes to stderr; reuse Maven's helper-style
        // approach inline here to avoid cross-adapter coupling.
        let output = std::process::Command::new("java")
            .arg("-version")
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
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

    fn default_tasks(&self) -> Vec<DefaultTask> {
        let inputs = vec![
            "src/**".into(),
            "build.gradle".into(),
            "build.gradle.kts".into(),
            "settings.gradle".into(),
            "settings.gradle.kts".into(),
            "gradle.properties".into(),
            "gradle/libs.versions.toml".into(),
        ];

        vec![
            DefaultTask {
                name: "build".into(),
                // -x test: tests run as their own task per bento's per-task
                // caching model; mixing them with build splits the cache key.
                run: "./gradlew build -x test".into(),
                inputs: Some(inputs.clone()),
                outputs: Some(vec![
                    "build/libs/**".into(),
                    "build/distributions/**".into(),
                ]),
            },
            DefaultTask {
                name: "test".into(),
                run: "./gradlew test".into(),
                inputs: Some({
                    let mut v = inputs.clone();
                    v.push("src/test/**".into());
                    v
                }),
                outputs: None,
            },
            DefaultTask {
                // `check` is Gradle's umbrella verification task —
                // includes whatever quality plugins (checkstyle, spotbugs,
                // detekt for Kotlin, etc.) are configured.
                name: "lint".into(),
                run: "./gradlew check -x test".into(),
                inputs: Some({
                    let mut v = inputs;
                    v.push("config/checkstyle/**".into());
                    v.push("config/detekt/**".into());
                    v
                }),
                outputs: None,
            },
        ]
    }
}

fn java(version: String) -> ToolVersion {
    ToolVersion {
        tool: "java".into(),
        version,
    }
}

/// Pick the Gradle invocation to use: the wrapper (`./gradlew`) when it
/// exists, system `gradle` otherwise. Returns `(program, owned_program)`
/// where `owned_program` is for diagnostics (the borrow lifetime ends at
/// function return).
fn invocation(dir: &Path) -> (&'static str, String) {
    if dir.join("gradlew").is_file() {
        ("./gradlew", "./gradlew".into())
    } else {
        ("gradle", "gradle".into())
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

/// Parse `[versions] java = "21"` (or `'21'`, or `21`) from the version
/// catalog. The `[versions]` table is the standard place; we accept any
/// `java` key in it.
fn parse_libs_versions_java(path: &Path) -> Result<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let value: toml::Value = match raw.parse() {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    let Some(java) = value.get("versions").and_then(|v| v.get("java")) else {
        return Ok(None);
    };
    // Accept string, integer, or float (`java = 21` parses as integer).
    let s = match java {
        toml::Value::String(s) => s.clone(),
        toml::Value::Integer(i) => i.to_string(),
        toml::Value::Float(f) => f.to_string(),
        _ => return Ok(None),
    };
    let trimmed = s.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

/// Parse `java.toolchain.languageVersion=N` (or `java.version=N`) from
/// gradle.properties. Some teams use these as a hand-rolled toolchain
/// pin without putting it in the build script.
fn parse_gradle_properties_java(path: &Path) -> Result<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    for raw_line in raw.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key == "java.toolchain.languageVersion" || key == "java.version" {
            let v = value.trim();
            if !v.is_empty() {
                return Ok(Some(v.to_string()));
            }
        }
    }
    Ok(None)
}

/// Best-effort regex over `build.gradle{.kts}` looking for the canonical
/// toolchain pin patterns. The Gradle DSL is too rich to parse properly
/// without an interpreter, so this catches the common shapes and falls
/// through quietly otherwise.
fn parse_build_gradle_java(path: &Path) -> Result<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(extract_java_from_build_gradle(&raw))
}

fn extract_java_from_build_gradle(content: &str) -> Option<String> {
    // Strip line comments — the patterns we look for are commonly
    // commented-out alternatives in real builds.
    let stripped = content
        .lines()
        .map(|l| l.split_once("//").map(|(c, _)| c).unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n");

    // Pattern 1: JavaLanguageVersion.of(N)
    if let Some(v) = find_after(&stripped, "JavaLanguageVersion.of(") {
        if let Some(end) = v.find(')') {
            let inner = v[..end].trim();
            if !inner.is_empty() {
                return Some(inner.to_string());
            }
        }
    }
    // Pattern 2: sourceCompatibility = JavaVersion.VERSION_N
    if let Some(v) = find_after(&stripped, "JavaVersion.VERSION_") {
        let token: String = v
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        // VERSION_17 → "17"; VERSION_1_8 → "1.8".
        let normalised = token.replace('_', ".");
        if !normalised.is_empty() {
            return Some(normalised);
        }
    }
    // Pattern 3: sourceCompatibility = '17' / "17" / 17
    for keyword in ["sourceCompatibility", "targetCompatibility"] {
        if let Some(v) = find_after(&stripped, keyword) {
            // Skip past `=` and whitespace.
            let v = v.trim_start();
            let v = v.strip_prefix('=').unwrap_or(v).trim_start();
            // Quoted form.
            if let Some(quote) = v.strip_prefix(['"', '\'']) {
                let close = quote.find(['"', '\''])?;
                let inner = &quote[..close];
                if !inner.is_empty() {
                    return Some(inner.to_string());
                }
            }
            // Bare form: take the leading digit/dot run.
            let bare: String = v
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if !bare.is_empty() {
                return Some(bare);
            }
        }
    }
    None
}

/// Return the substring after the first occurrence of `needle`, or None.
fn find_after<'a>(haystack: &'a str, needle: &str) -> Option<&'a str> {
    haystack.find(needle).map(|i| &haystack[i + needle.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            let p = dir.path().join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, content).unwrap();
        }
        dir
    }

    #[test]
    fn id_and_fingerprint() {
        let a = GradleAdapter;
        assert_eq!(a.id(), "gradle");
        let fp = a.fingerprint_files();
        for f in [
            "build.gradle",
            "build.gradle.kts",
            "settings.gradle",
            "gradle.properties",
            "gradle/libs.versions.toml",
            ".tool-versions",
        ] {
            assert!(fp.iter().any(|s| s == f), "fingerprint missing: {f}");
        }
    }

    #[test]
    fn detect_finds_groovy_build_gradle() {
        let tmp = tmp_with(&[("build.gradle", "plugins { id 'java' }\n")]);
        assert!(GradleAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_finds_kotlin_build_gradle() {
        let tmp = tmp_with(&[("build.gradle.kts", "plugins { java }\n")]);
        assert!(GradleAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_finds_settings_only_root() {
        // Multi-module setups can have just settings.gradle at root.
        let tmp = tmp_with(&[("settings.gradle.kts", "rootProject.name = \"app\"\n")]);
        assert!(GradleAdapter.detect(tmp.path()));
    }

    #[test]
    fn detect_rejects_pom_only_dir() {
        let tmp = tmp_with(&[("pom.xml", "<project/>")]);
        assert!(!GradleAdapter.detect(tmp.path()));
    }

    #[test]
    fn toolchain_prefers_java_version_file() {
        let tmp = tmp_with(&[
            (".java-version", "21\n"),
            (
                "build.gradle.kts",
                "java { toolchain { languageVersion = JavaLanguageVersion.of(17) } }\n",
            ),
            ("gradle/libs.versions.toml", "[versions]\njava = \"19\"\n"),
        ]);
        let v = GradleAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.tool, "java");
        assert_eq!(v.version, "21");
    }

    #[test]
    fn toolchain_falls_back_to_libs_versions_toml() {
        let tmp = tmp_with(&[
            ("build.gradle.kts", "plugins { java }\n"),
            (
                "gradle/libs.versions.toml",
                "[versions]\njava = \"21\"\nkotlin = \"2.0.0\"\n",
            ),
        ]);
        let v = GradleAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.version, "21");
    }

    #[test]
    fn toolchain_libs_versions_accepts_integer() {
        let tmp = tmp_with(&[
            ("build.gradle.kts", "plugins { java }\n"),
            ("gradle/libs.versions.toml", "[versions]\njava = 17\n"),
        ]);
        let v = GradleAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.version, "17");
    }

    #[test]
    fn toolchain_falls_back_to_gradle_properties() {
        let tmp = tmp_with(&[
            ("build.gradle.kts", "plugins { java }\n"),
            (
                "gradle.properties",
                "# pin\njava.toolchain.languageVersion=21\n",
            ),
        ]);
        let v = GradleAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.version, "21");
    }

    #[test]
    fn toolchain_extracts_java_language_version_kts() {
        let tmp = tmp_with(&[(
            "build.gradle.kts",
            "java {\n  toolchain {\n    languageVersion = JavaLanguageVersion.of(21)\n  }\n}\n",
        )]);
        let v = GradleAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.version, "21");
    }

    #[test]
    fn toolchain_extracts_source_compatibility_quoted() {
        let tmp = tmp_with(&[(
            "build.gradle",
            "apply plugin: 'java'\nsourceCompatibility = '17'\n",
        )]);
        let v = GradleAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.version, "17");
    }

    #[test]
    fn toolchain_extracts_source_compatibility_javaversion_const() {
        let tmp = tmp_with(&[(
            "build.gradle",
            "sourceCompatibility = JavaVersion.VERSION_17\n",
        )]);
        let v = GradleAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.version, "17");
    }

    #[test]
    fn toolchain_extracts_javaversion_const_dotted_form() {
        let tmp = tmp_with(&[(
            "build.gradle",
            "sourceCompatibility = JavaVersion.VERSION_1_8\n",
        )]);
        let v = GradleAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.version, "1.8");
    }

    #[test]
    fn toolchain_falls_back_to_tool_versions() {
        let tmp = tmp_with(&[
            ("build.gradle.kts", "plugins { java }\n"),
            (".tool-versions", "java 21.0.2-tem\n"),
        ]);
        let v = GradleAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.version, "21.0.2-tem");
    }

    #[test]
    fn toolchain_returns_none_when_unpinned() {
        let tmp = tmp_with(&[("build.gradle.kts", "plugins { java }\n")]);
        assert!(GradleAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .is_none());
    }

    #[test]
    fn toolchain_skips_commented_pin_in_build_gradle() {
        let tmp = tmp_with(&[(
            "build.gradle.kts",
            "// languageVersion = JavaLanguageVersion.of(11)\njava {\n  toolchain {\n    languageVersion = JavaLanguageVersion.of(21)\n  }\n}\n",
        )]);
        let v = GradleAdapter
            .required_toolchain(tmp.path())
            .unwrap()
            .unwrap();
        assert_eq!(v.version, "21");
    }

    #[test]
    fn invocation_prefers_wrapper_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("gradlew"), "#!/bin/sh\n").unwrap();
        let (program, _) = invocation(tmp.path());
        assert_eq!(program, "./gradlew");
    }

    #[test]
    fn invocation_falls_back_to_system_gradle_without_wrapper() {
        let tmp = tempfile::tempdir().unwrap();
        let (program, _) = invocation(tmp.path());
        assert_eq!(program, "gradle");
    }

    #[test]
    fn default_tasks_use_gradlew_lifecycle() {
        let tasks = GradleAdapter.default_tasks();
        let names: Vec<_> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["build", "test", "lint"]);
        assert_eq!(tasks[0].run, "./gradlew build -x test");
        assert_eq!(tasks[1].run, "./gradlew test");
        assert_eq!(tasks[2].run, "./gradlew check -x test");
    }
}
