//! Vercel integration.
//!
//! - Detects: `vercel.json` or `.vercel/project.json` at the dish root.
//! - Required env: `VERCEL_TOKEN` (auth); `VERCEL_ORG_ID` and
//!   `VERCEL_PROJECT_ID` are also commonly needed but Vercel's CLI
//!   falls back to inferring from `.vercel/` — we only hard-require
//!   the token.
//! - Emits: `vercel:deploy` (prod), `vercel:preview` (staging).
//!
//! Build is expected to run *before* deploy. We don't emit a
//! `depends_on = ["build"]` hard requirement because:
//!   (a) not every Vercel project uses bento's build task (some let
//!       Vercel build from the git sha server-side),
//!   (b) bento's task ordering is list-position-based, so when
//!       `bento ci` runs, build lands before deploy anyway.
//! Users wanting a prior build run should use `bento deploy` which
//! the CLI wires to "build + deploy-kind" via `task_kind_filter`.

use std::path::Path;

use crate::integration::{CliRequirement, Integration, IntegrationTask, IntegrationTaskKind};

pub struct VercelIntegration;

impl Integration for VercelIntegration {
    fn id(&self) -> &str {
        "vercel"
    }

    fn display_name(&self) -> &str {
        "Vercel"
    }

    fn detect(&self, dir: &Path) -> bool {
        dir.join("vercel.json").is_file() || dir.join(".vercel").join("project.json").is_file()
    }

    fn required_env(&self) -> Vec<String> {
        vec!["VERCEL_TOKEN".into()]
    }

    fn required_cli(&self) -> Vec<CliRequirement> {
        vec![CliRequirement::new(
            "vercel",
            "npm install -g vercel  (or see https://vercel.com/docs/cli)",
        )]
    }

    fn detected_tasks(&self, _dir: &Path, _config: &toml::Table) -> Vec<IntegrationTask> {
        vec![
            IntegrationTask {
                name: "vercel:deploy".into(),
                kind: IntegrationTaskKind::Deploy,
                run: "vercel deploy --prod --yes --token $VERCEL_TOKEN".into(),
                depends_on: vec!["build".into()],
                env_vars: vec![
                    "VERCEL_TOKEN".into(),
                    "VERCEL_ORG_ID".into(),
                    "VERCEL_PROJECT_ID".into(),
                ],
                no_cache: true,
                outputs: Vec::new(),
            },
            IntegrationTask {
                name: "vercel:preview".into(),
                kind: IntegrationTaskKind::DeployPreview,
                run: "vercel deploy --yes --token $VERCEL_TOKEN".into(),
                depends_on: vec!["build".into()],
                env_vars: vec![
                    "VERCEL_TOKEN".into(),
                    "VERCEL_ORG_ID".into(),
                    "VERCEL_PROJECT_ID".into(),
                ],
                no_cache: true,
                outputs: Vec::new(),
            },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, content) in files {
            let full = dir.path().join(name);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&full, content).unwrap();
        }
        dir
    }

    #[test]
    fn id_and_required_env() {
        assert_eq!(VercelIntegration.id(), "vercel");
        assert_eq!(VercelIntegration.display_name(), "Vercel");
        assert_eq!(VercelIntegration.required_env(), vec!["VERCEL_TOKEN"]);
    }

    #[test]
    fn detect_matches_vercel_json() {
        let tmp = tmp_with(&[("vercel.json", "{}")]);
        assert!(VercelIntegration.detect(tmp.path()));
    }

    #[test]
    fn detect_matches_dot_vercel_project_json() {
        let tmp = tmp_with(&[(".vercel/project.json", r#"{"projectId":"x"}"#)]);
        assert!(VercelIntegration.detect(tmp.path()));
    }

    #[test]
    fn detect_false_for_unrelated_project() {
        let tmp = tmp_with(&[("package.json", "{}")]);
        assert!(!VercelIntegration.detect(tmp.path()));
    }

    #[test]
    fn emits_deploy_and_preview_tasks() {
        let tmp = tempfile::tempdir().unwrap();
        let config = toml::Table::new();
        let tasks = VercelIntegration.detected_tasks(tmp.path(), &config);
        let names: Vec<_> = tasks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["vercel:deploy", "vercel:preview"]);
        assert_eq!(tasks[0].kind, IntegrationTaskKind::Deploy);
        assert_eq!(tasks[1].kind, IntegrationTaskKind::DeployPreview);
        // Both deploys short-circuit the cache.
        assert!(tasks[0].no_cache);
        assert!(tasks[1].no_cache);
        // Both depend on build (advisory metadata).
        assert_eq!(tasks[0].depends_on, vec!["build"]);
    }
}
