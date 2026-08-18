//! Deno adapter.
//!
//! - Detects: `deno.json`, `deno.jsonc`, or `deno.lock` at the dish root.
//! - Fingerprints: `deno.json`, `deno.jsonc`, `deno.lock`, `import_map.json`.
//! - Toolchain pin: `.deno-version` (convention) or the `deno` field in
//!   `deno.json` if present. No `engines` fallback — Deno projects don't
//!   mix Node toolchains the way Bun projects sometimes do.
//! - Install: `deno install --frozen=true` when a lockfile exists;
//!   otherwise a no-op (Deno fetches on demand).
//! - Default tasks: prefer `deno task <name>` when the dish declares
//!   `tasks.{build,test,lint}` in `deno.json`; otherwise fall back to
//!   `deno check .`, `deno test`, and `deno lint`.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use crate::adapter::{DefaultTask, DetectedTask, LanguageAdapter, TaskContext, ToolVersion};

pub struct DenoAdapter;

const FINGERPRINT: &[&str] = &[
    "deno.json",
    "deno.jsonc",
    "deno.lock",
    "import_map.json",
    ".deno-version",
    ".tool-versions",
];

impl LanguageAdapter for DenoAdapter {
    fn id(&self) -> &str {
        "deno"
    }

    fn detect(&self, dir: &Path) -> bool {
        dir.join("deno.json").is_file()
            || dir.join("deno.jsonc").is_file()
            || dir.join("deno.lock").is_file()
    }

    fn fingerprint_files(&self) -> Vec<String> {
        FINGERPRINT.iter().map(|s| (*s).to_string()).collect()
    }

    fn required_toolchain(&self, dir: &Path) -> Result<Option<ToolVersion>> {
        // 1. `.deno-version` — community convention (mirrors `.nvmrc`).
        let dot = dir.join(".deno-version");
        if dot.is_file() {
            let raw = std::fs::read_to_string(&dot)
                .with_context(|| format!("reading {}", dot.display()))?;
            let line = raw.lines().next().unwrap_or("").trim();
            let stripped = line.strip_prefix('v').unwrap_or(line);
            if !stripped.is_empty() {
                return Ok(Some(ToolVersion {
                    tool: "deno".into(),
                    version: stripped.to_string(),
                }));
            }
        }
        // 2. `deno` field in deno.json.
        if let Some(ver) = read_deno_config(dir)
            .as_ref()
            .and_then(|v| v.get("deno"))
            .and_then(|v| v.as_str())
        {
            let t = ver.trim();
            if !t.is_empty() {
                return Ok(Some(ToolVersion {
                    tool: "deno".into(),
                    version: t.to_string(),
                }));
            }
        }
        // 3. .tool-versions (asdf/mise).
        if let Some(v) = crate::tool_versions::read_tool_version(dir, &["deno"])? {
            return Ok(Some(ToolVersion {
                tool: "deno".into(),
                version: v,
            }));
        }
        Ok(None)
    }

    fn install(&self, ctx: &TaskContext) -> Result<()> {
        // Only run `deno install` when a lockfile exists; Deno fetches
        // on demand otherwise and an eager install would do nothing useful.
        if !ctx.dish_dir.join("deno.lock").is_file() {
            return Ok(());
        }
        let mut cmd = Command::new("deno");
        cmd.args(["install", "--frozen=true"]);
        ctx.apply_env(&mut cmd);
        crate::adapter::run_install_cmd(ctx, &mut cmd, "deno install")
    }

    fn resolved_toolchain_fingerprint(&self) -> Option<String> {
        crate::probe::memoised("deno", &["--version"])
    }

    fn detected_tasks(&self, dir: &Path) -> Option<Vec<DetectedTask>> {
        let value = read_deno_config(dir)?;
        let tasks = value.get("tasks")?.as_object()?;
        Some(
            tasks
                .keys()
                .map(|k| k.trim())
                .filter(|n| !n.is_empty())
                .map(|n| DetectedTask {
                    name: n.to_string(),
                    run: format!("deno task {n}"),
                })
                .collect(),
        )
    }

    fn default_tasks(&self, dir: &Path) -> Vec<DefaultTask> {
        let declared = declared_tasks(dir);
        let task_or = |name: &str, fallback: &str| {
            if declared.iter().any(|d| d == name) {
                format!("deno task {name}")
            } else {
                fallback.to_string()
            }
        };
        let inputs = vec![
            "src/**".into(),
            "**/*.ts".into(),
            "**/*.tsx".into(),
            "deno.json".into(),
            "deno.jsonc".into(),
            "deno.lock".into(),
            "import_map.json".into(),
        ];

        vec![
            DefaultTask {
                name: "build".into(),
                // `deno check` is the closest analogue to a build step — it
                // type-checks the graph without emitting artefacts.
                run: task_or("build", "deno check ."),
                inputs: Some(inputs.clone()),
                outputs: Some(vec!["dist/**".into()]),
            },
            DefaultTask {
                name: "test".into(),
                run: task_or("test", "deno test --allow-read"),
                inputs: Some(inputs.clone()),
                outputs: None,
            },
            DefaultTask {
                name: "lint".into(),
                run: task_or("lint", "deno lint"),
                inputs: Some(inputs),
                outputs: None,
            },
        ]
    }
}

/// Parse `deno.json` (then `deno.jsonc`). Best-effort: an unreadable or
/// malformed config yields `None` so the rest of the adapter keeps
/// working rather than erroring the whole plan.
fn read_deno_config(dir: &Path) -> Option<serde_json::Value> {
    for file in ["deno.json", "deno.jsonc"] {
        let path = dir.join(file);
        if !path.is_file() {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(value) = serde_json::from_str(&strip_jsonc_comments(&raw)) {
            return Some(value);
        }
    }
    None
}

fn declared_tasks(dir: &Path) -> Vec<String> {
    read_deno_config(dir)
        .as_ref()
        .and_then(|v| v.get("tasks"))
        .and_then(|t| t.as_object())
        .map(|t| t.keys().map(|k| k.trim().to_string()).collect())
        .unwrap_or_default()
}

/// Very small JSONC stripper: drops `//`-to-EOL and `/* … */` comments.
/// Sufficient for reading a top-level `deno` key out of `deno.jsonc`;
/// pathological cases (comment markers inside strings) still work because
/// we're careful to track the in-string state.
fn strip_jsonc_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    let mut in_string = false;
    let mut escape = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                chars.next();
                while let Some(&nc) = chars.peek() {
                    if nc == '\n' {
                        break;
                    }
                    chars.next();
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = '\0';
                for nc in chars.by_ref() {
                    if prev == '*' && nc == '/' {
                        break;
                    }
                    prev = nc;
                }
            }
            _ => out.push(c),
        }
    }
    out
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
        let a = DenoAdapter;
        assert_eq!(a.id(), "deno");
        let fp = a.fingerprint_files();
        assert!(fp.iter().any(|s| s == "deno.json"));
        assert!(fp.iter().any(|s| s == "deno.lock"));
    }

    #[test]
    fn detect_accepts_deno_json_or_lockfile() {
        for marker in ["deno.json", "deno.jsonc", "deno.lock"] {
            let tmp = tmp_with(&[(marker, "{}")]);
            assert!(DenoAdapter.detect(tmp.path()), "didn't detect via {marker}");
        }
    }

    #[test]
    fn detect_returns_false_for_node_project() {
        let tmp = tmp_with(&[("package.json", r#"{}"#)]);
        assert!(!DenoAdapter.detect(tmp.path()));
    }

    #[test]
    fn toolchain_reads_dot_deno_version() {
        let tmp = tmp_with(&[("deno.json", "{}"), (".deno-version", "v2.0.3\n")]);
        let v = DenoAdapter.required_toolchain(tmp.path()).unwrap().unwrap();
        assert_eq!(v.tool, "deno");
        assert_eq!(v.version, "2.0.3");
    }

    #[test]
    fn toolchain_reads_deno_field_in_deno_json() {
        let tmp = tmp_with(&[("deno.json", r#"{ "deno": "1.46.0" }"#)]);
        let v = DenoAdapter.required_toolchain(tmp.path()).unwrap().unwrap();
        assert_eq!(v.version, "1.46.0");
    }

    #[test]
    fn toolchain_handles_jsonc_comments() {
        let tmp = tmp_with(&[(
            "deno.jsonc",
            r#"{
                // pinned for CI reproducibility
                "deno": "1.46.0",
                /* inline comment */
                "tasks": {}
            }"#,
        )]);
        let v = DenoAdapter.required_toolchain(tmp.path()).unwrap().unwrap();
        assert_eq!(v.version, "1.46.0");
    }

    #[test]
    fn strip_jsonc_comments_preserves_strings() {
        let s = strip_jsonc_comments(r#"{"u": "http://x/y", "a": 1 /* c */}"#);
        assert!(s.contains(r#""http://x/y""#));
        assert!(!s.contains("/* c */"));
    }

    #[test]
    fn default_tasks_use_deno_commands() {
        let tmp = tmp_with(&[("deno.json", "{}")]);
        let tasks = DenoAdapter.default_tasks(tmp.path());
        assert_eq!(tasks[0].name, "build");
        // No `||`: a failing build must fail the task, not fall through
        // to a type-check that passes.
        assert_eq!(tasks[0].run, "deno check .");
        assert_eq!(tasks[1].run, "deno test --allow-read");
        assert_eq!(tasks[2].run, "deno lint");
    }

    #[test]
    fn default_tasks_prefer_tasks_declared_in_deno_json() {
        let tmp = tmp_with(&[(
            "deno.json",
            r#"{"tasks": {"build": "deno bundle main.ts", "lint": "deno lint --rules"}}"#,
        )]);
        let tasks = DenoAdapter.default_tasks(tmp.path());
        assert_eq!(tasks[0].run, "deno task build");
        assert_eq!(tasks[1].run, "deno test --allow-read");
        assert_eq!(tasks[2].run, "deno task lint");
    }

    #[test]
    fn detected_tasks_mirror_deno_json_tasks() {
        let tmp = tmp_with(&[(
            "deno.jsonc",
            r#"{
                // the project's own wiring
                "tasks": {"build": "deno check main.ts", "dev": "deno run -A main.ts"}
            }"#,
        )]);
        let mut tasks = DenoAdapter.detected_tasks(tmp.path()).unwrap();
        tasks.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].name, "build");
        assert_eq!(tasks[0].run, "deno task build");
        assert_eq!(tasks[1].run, "deno task dev");
    }

    #[test]
    fn detected_tasks_none_without_a_tasks_block() {
        let tmp = tmp_with(&[("deno.json", r#"{"name": "x"}"#)]);
        assert!(DenoAdapter.detected_tasks(tmp.path()).is_none());
    }
}
