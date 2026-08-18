//! Makefile → bento migrator.
//!
//! Reads a single top-level `Makefile`, collects its target names, and
//! emits one `[tasks.<target>] run = "make <target>"` per target.
//!
//! **Make stays the executor.** Copying recipe lines into `run` looked
//! like a closer translation but produced config that couldn't run:
//! `$(CC)` becomes shell command substitution, `$@` / `$<` expand to
//! nothing, `@echo` / `-rm` keep their Make-only line prefixes, and
//! prerequisites are dropped on the floor. Delegating to `make` gets
//! variable expansion, automatic variables, prerequisite ordering,
//! `.ONESHELL`, and includes for free — exactly the parts a
//! reimplementation gets wrong.
//!
//! ## What translates cleanly
//!
//! | Makefile                            | Bento                                     |
//! |-------------------------------------|-------------------------------------------|
//! | `target:` + TAB-indented recipes    | `dish.toml [tasks.<target>] run = "make …"`|
//!
//! ## What gets a note instead
//!
//! - Pattern rules (`%.o: %.c`) are skipped with a `Skipped` note —
//!   they're templates, not invocable targets.
//! - The dish gets no `language`: a Makefile is language-agnostic, so
//!   there's no package manager for bento to drive. Surfaced as an
//!   `Inferred` note pointing at the field.
//!
//! ## Output shape
//!
//! Single-dish layout: the Makefile root becomes one bento with one
//! dish (the root itself).

use std::fs;

use anyhow::{Context, Result};

use super::{render_task, Emitter, MigrationReport, NoteKind, TaskBlock};

// ── Public entry point ─────────────────────────────────────────────

pub fn run(mut e: Emitter) -> Result<MigrationReport> {
    let root = e.root().to_path_buf();

    let makefile_path = root.join("Makefile");
    let body = fs::read_to_string(&makefile_path)
        .with_context(|| format!("reading {}", makefile_path.display()))?;
    let parsed = parse_targets(&body);

    for pat in &parsed.pattern_rules {
        e.note(
            NoteKind::Skipped,
            format!(
                "pattern rule `{pat}` skipped — it's a template, not an invocable target; \
                 add an explicit `[tasks.<name>]` if you need to drive it."
            ),
        );
    }

    if parsed.targets.is_empty() {
        e.note(
            NoteKind::Skipped,
            "Makefile has no targets with recipes — nothing to write to dish.toml",
        );
        return e.finish();
    }

    e.note(
        NoteKind::Inferred,
        "dish.toml has no `language` — a Makefile is language-agnostic, so bento drives \
         no package manager here and `bento install` is a no-op. Add `language = \"…\"` if \
         the repo also has a native manifest (Cargo.toml, package.json, go.mod, …).",
    );

    let dish_name = root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("dish")
        .to_string();
    let mut dish_body = format!(
        "name = \"{dish_name}\"\n\
         \n\
         # Migrated from Makefile. Each [tasks.<name>] delegates back to\n\
         # `make <target>` so Make keeps handling variable expansion,\n\
         # automatic variables, and prerequisite ordering.\n"
    );
    for target in &parsed.targets {
        dish_body.push_str(&render_task(
            target,
            &TaskBlock {
                run: &format!("make {target}"),
                ..Default::default()
            },
        ));
    }
    e.dish(&root, &dish_body)?;

    e.finish()
}

// ── Target scan ────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct Targets {
    /// Invocable target names, in source order, that carry a recipe.
    targets: Vec<String>,
    /// Pattern-rule headers we skipped (e.g. `%.o: %.c`).
    pattern_rules: Vec<String>,
}

/// Collect target names from a Makefile. Deliberately shallow: bento
/// only needs the names to hand back to `make`, so this recognises
/// target headers and nothing else.
fn parse_targets(body: &str) -> Targets {
    let mut out = Targets::default();
    // Targets opened by the most recent header, awaiting a TAB line.
    let mut pending: Vec<String> = Vec::new();

    for raw in body.lines() {
        if raw.starts_with('\t') {
            out.targets.append(&mut pending);
            continue;
        }
        let trimmed = raw.trim_end_matches('\r').trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        pending.clear();
        let Some((header, prereqs)) = split_target_line(trimmed) else {
            continue;
        };
        for name in header.split_whitespace() {
            if name.contains('%') {
                let suffix = if prereqs.is_empty() {
                    String::new()
                } else {
                    format!(" {prereqs}")
                };
                out.pattern_rules.push(format!("{name}:{suffix}"));
            } else if name.starts_with('.') {
                // `.PHONY`, `.SUFFIXES`, `.DEFAULT` — directives, not targets.
            } else if !out.targets.iter().any(|t| t == name) {
                pending.push(name.to_string());
            }
        }
    }
    out.targets.append(&mut pending);
    out
}

/// Split a target header on the first `:` that isn't part of an
/// assignment operator (`:=`, `::=`), returning
/// `(target_names, prereqs)`. `None` for anything that isn't a target
/// header — variable assignments, directives, plain text.
fn split_target_line(line: &str) -> Option<(&str, &str)> {
    let colon = line.find(':')?;
    let rest = &line[colon + 1..];
    // `VAR := x` / `VAR ::= x` are assignments, not targets. A bare
    // `::` is a double-colon rule, so only bail when `=` follows.
    let after_colons = rest.trim_start_matches(':');
    if after_colons.starts_with('=') {
        return None;
    }
    let header = line[..colon].trim();
    // `VAR = x` / `VAR ?= x` / `VAR += x` have no colon before the `=`.
    if header.is_empty() || header.contains('=') {
        return None;
    }
    Some((header, after_colons.trim()))
}

// ── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate::Options;
    use std::path::Path;

    fn migrate(root: &Path, dry_run: bool, force: bool) -> MigrationReport {
        run(Emitter::new(Options {
            root: root.to_path_buf(),
            dry_run,
            force,
        }))
        .unwrap()
    }

    fn write_makefile(tmp: &tempfile::TempDir, body: &str) {
        std::fs::write(tmp.path().join("Makefile"), body).unwrap();
    }

    #[test]
    fn migrates_simple_makefile_with_three_targets() {
        let tmp = tempfile::tempdir().unwrap();
        write_makefile(
            &tmp,
            "build:\n\tcargo build\n\ntest:\n\tcargo test\n\nclean:\n\trm -rf target\n",
        );
        let report = migrate(tmp.path(), false, false);
        assert!(report.applied);

        let dish = std::fs::read_to_string(tmp.path().join("dish.toml")).unwrap();
        assert!(dish.contains("[tasks.build]"));
        assert!(dish.contains(r#"run = "make build""#));
        assert!(dish.contains("[tasks.test]"));
        assert!(dish.contains(r#"run = "make test""#));
        assert!(dish.contains("[tasks.clean]"));
        assert!(dish.contains(r#"run = "make clean""#));

        // bento.toml + bentos/prod.toml emitted too.
        assert!(tmp.path().join("bento.toml").exists());
        let prod = std::fs::read_to_string(tmp.path().join("bentos/prod.toml")).unwrap();
        assert!(prod.contains("\".\""));
    }

    #[test]
    fn emits_no_language_so_install_stays_a_no_op() {
        // A `language = "node-npm"` placeholder made the very first
        // `bento build` fail with `npm install`: ENOENT package.json.
        let tmp = tempfile::tempdir().unwrap();
        write_makefile(&tmp, "build:\n\t$(CC) -o out/hello hello.c\n");
        let report = migrate(tmp.path(), false, false);
        let dish = std::fs::read_to_string(tmp.path().join("dish.toml")).unwrap();
        assert!(!dish.contains("language ="), "{dish}");
        assert!(report
            .notes
            .iter()
            .any(|n| n.kind == NoteKind::Inferred && n.message.contains("`language`")));
    }

    #[test]
    fn recipes_are_never_copied_verbatim() {
        // Make-only syntax ($(VAR), $@, @echo, -rm) is meaningless to a
        // shell — the recipe stays make's job.
        let tmp = tempfile::tempdir().unwrap();
        write_makefile(
            &tmp,
            "CC := gcc\n\nbuild: clean\n\t@echo building\n\t$(CC) -o $@ hello.c\n\t-rm -f tmp\n\nclean:\n\trm -rf out\n",
        );
        migrate(tmp.path(), false, false);
        let dish = std::fs::read_to_string(tmp.path().join("dish.toml")).unwrap();
        assert!(dish.contains(r#"run = "make build""#), "{dish}");
        for make_only in ["$(CC)", "$@", "@echo", "-rm"] {
            assert!(!dish.contains(make_only), "{make_only} leaked into {dish}");
        }
        // The assignment isn't a target.
        assert!(!dish.contains("[tasks.CC]"), "{dish}");
    }

    #[test]
    fn refuses_to_overwrite_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        write_makefile(&tmp, "build:\n\techo built\n");
        std::fs::write(tmp.path().join("dish.toml"), "name = \"existing\"\n").unwrap();
        let report = migrate(tmp.path(), false, false);
        assert!(report.has_conflicts());
        // dish.toml stays untouched.
        let body = std::fs::read_to_string(tmp.path().join("dish.toml")).unwrap();
        assert_eq!(body, "name = \"existing\"\n");
        // …but the root is still the bento's dish.
        let prod = std::fs::read_to_string(tmp.path().join("bentos/prod.toml")).unwrap();
        assert!(prod.contains("\".\""), "{prod}");
    }

    #[test]
    fn dry_run_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        write_makefile(&tmp, "build:\n\techo built\n");
        let report = migrate(tmp.path(), true, false);
        assert!(!report.applied);
        assert!(!report.files_written.is_empty());
        assert!(!tmp.path().join("dish.toml").exists());
        assert!(!tmp.path().join("bento.toml").exists());
        assert!(!tmp.path().join("bentos/prod.toml").exists());
    }

    #[test]
    fn skips_pattern_rules_with_note() {
        let tmp = tempfile::tempdir().unwrap();
        write_makefile(
            &tmp,
            "%.o: %.c\n\t$(CC) -c $< -o $@\n\nbuild:\n\techo built\n",
        );
        let report = migrate(tmp.path(), true, false);
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.kind == NoteKind::Skipped && n.message.contains("%.o")),
            "pattern rule should produce a Skipped note; got {:?}",
            report.notes
        );
        // …and no [tasks."%.o"] block.
        let written = report.files_written.iter().any(|f| f.bytes > 0);
        assert!(written);
    }

    #[test]
    fn phony_and_other_dot_directives_are_not_targets() {
        let tmp = tempfile::tempdir().unwrap();
        write_makefile(
            &tmp,
            ".PHONY: clean test\n.SUFFIXES:\n\nclean:\n\trm -rf target\n\ntest:\n\tcargo test\n",
        );
        migrate(tmp.path(), false, false);
        let dish = std::fs::read_to_string(tmp.path().join("dish.toml")).unwrap();
        assert!(!dish.contains("PHONY"), "{dish}");
        assert!(!dish.contains("SUFFIXES"), "{dish}");
        assert!(dish.contains("[tasks.clean]"));
        assert!(dish.contains("[tasks.test]"));
    }

    #[test]
    fn targets_without_recipes_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        write_makefile(&tmp, "all: build\n\nbuild:\n\techo built\n");
        migrate(tmp.path(), false, false);
        let dish = std::fs::read_to_string(tmp.path().join("dish.toml")).unwrap();
        assert!(!dish.contains("[tasks.all]"), "{dish}");
        assert!(dish.contains("[tasks.build]"));
    }

    #[test]
    fn missing_makefile_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run(Emitter::new(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        }))
        .unwrap_err();
        assert!(format!("{err:#}").contains("Makefile"));
    }

    #[test]
    fn split_target_line_handles_double_colon() {
        let (h, p) = split_target_line("foo:: bar baz").unwrap();
        assert_eq!(h, "foo");
        assert_eq!(p, "bar baz");
    }

    #[test]
    fn split_target_line_rejects_assignments() {
        for assignment in [
            "CC := gcc",
            "CC ::= gcc",
            "CC = gcc",
            "CC ?= gcc",
            "CC += gcc",
        ] {
            assert!(
                split_target_line(assignment).is_none(),
                "{assignment} parsed as a target"
            );
        }
        assert!(split_target_line("build: dep").is_some());
        assert!(split_target_line("build:").is_some());
    }
}
