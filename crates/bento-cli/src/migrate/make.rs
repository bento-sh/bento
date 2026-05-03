//! Makefile → bento migrator.
//!
//! Reads a single top-level `Makefile`, scans for target/recipe pairs,
//! and emits a starter bento config. This is **best-effort** — Make's
//! semantics (variable expansion, pattern rules, automatic variables,
//! conditional directives) are far richer than what bento models, so
//! anything we can't faithfully translate is surfaced as a note for
//! the user to handle by hand.
//!
//! ## What translates cleanly
//!
//! | Makefile                            | Bento                              |
//! |-------------------------------------|------------------------------------|
//! | `target:` + TAB-indented recipes    | `dish.toml [tasks.<target>] run`   |
//! | Single-line recipe                  | `run = "<line>"`                   |
//! | Multi-line recipe                   | `run = "line1 && line2 && line3"`  |
//! | `.PHONY: a b c`                     | Surfaced as a note                 |
//!
//! ## What gets a note instead
//!
//! - `$(VAR)` / `${VAR}` expansions are passed through verbatim — bento
//!   doesn't expand Make variables. One `Inferred` note covers the lot.
//! - Pattern rules (`%.o: %.c`) are skipped with a `Skipped` note —
//!   bento has no equivalent.
//! - Automatic variables (`$@`, `$<`, `$^`) are passed through verbatim
//!   but listed in an `Inferred` note so the user knows to substitute.
//! - Prerequisites on a target (`build: foo bar`) are noted — bento
//!   models task ordering via dish-level `depends_on`, not per-target.
//! - Variable assignments (`CC := gcc`) are skipped silently — they
//!   live above the targets and don't have a bento analogue.
//!
//! ## Output shape
//!
//! Single-dish layout: the Makefile root becomes one bento with one
//! dish (the root itself).
//!
//! - `dish.toml` at the root with `name = "<basename>"`,
//!   `language = "node-npm"` (placeholder — Make is language-agnostic;
//!   a note flags this for the user to fix).
//! - Root `bento.toml` via `crate::init::render_bento_toml`.
//! - `bentos/prod.toml` via `crate::init::render_prod_toml(&["."])`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::init::{toml_basic_string, toml_table_key};

use super::{MigrationReport, NoteKind};

// ── Public entry point ─────────────────────────────────────────────

pub struct Options {
    pub root: PathBuf,
    pub dry_run: bool,
    pub force: bool,
}

pub fn run(opts: Options) -> Result<MigrationReport> {
    let mut report = MigrationReport {
        applied: !opts.dry_run,
        ..Default::default()
    };

    // 1. Load the Makefile.
    let makefile_path = opts.root.join("Makefile");
    let body = fs::read_to_string(&makefile_path)
        .with_context(|| format!("reading {}", makefile_path.display()))?;

    // 2. Parse — collect targets, recipes, .PHONY list, pattern rules.
    let parsed = parse_makefile(&body);

    if parsed.targets.is_empty() && parsed.pattern_rules.is_empty() {
        report.push_note(
            NoteKind::Skipped,
            "Makefile has no targets — nothing to migrate",
        );
        return Ok(report);
    }

    // 3. Surface notes for things we don't translate cleanly.
    for pat in &parsed.pattern_rules {
        report.push_note(
            NoteKind::Skipped,
            format!(
                "pattern rule `{pat}` skipped — bento has no equivalent for Make pattern rules; \
                 if the rule matters, hand-port it as an explicit `[tasks.<name>]` block."
            ),
        );
    }
    if !parsed.phony.is_empty() {
        report.push_note(
            NoteKind::Inferred,
            format!(
                ".PHONY targets {:?} declared in Makefile — bento tasks always run when invoked, \
                 so the .PHONY distinction has no direct analogue. Listed for awareness.",
                parsed.phony
            ),
        );
    }
    let mut targets_with_prereqs: Vec<&str> = parsed
        .targets
        .iter()
        .filter(|t| !t.prereqs.is_empty())
        .map(|t| t.name.as_str())
        .collect();
    targets_with_prereqs.sort();
    if !targets_with_prereqs.is_empty() {
        report.push_note(
            NoteKind::Inferred,
            format!(
                "targets {:?} declared prerequisites — bento doesn't model intra-task dependencies; \
                 chain them via dish-level `depends_on` between dishes, or compose the recipes \
                 manually inside the `run` field.",
                targets_with_prereqs,
            ),
        );
    }
    let mut auto_var_targets: Vec<&str> = parsed
        .targets
        .iter()
        .filter(|t| recipe_uses_automatic_var(&t.recipe))
        .map(|t| t.name.as_str())
        .collect();
    auto_var_targets.sort();
    if !auto_var_targets.is_empty() {
        report.push_note(
            NoteKind::Inferred,
            format!(
                "targets {:?} use Make automatic variables (`$@`, `$<`, `$^`, …) — passed \
                 through verbatim. bento doesn't expand them; substitute concrete paths or \
                 wrap the recipe in a `make <target>` invocation.",
                auto_var_targets,
            ),
        );
    }
    if parsed.has_variable_expansion {
        report.push_note(
            NoteKind::Inferred,
            "Make variable expansions (`$(VAR)`, `${VAR}`) left literal — review and substitute \
             manually if needed.",
        );
    }

    // 4. Pick the targets that actually have a runnable recipe.
    let runnable: Vec<&Target> = parsed
        .targets
        .iter()
        .filter(|t| !t.recipe.is_empty())
        .collect();

    if runnable.is_empty() {
        report.push_note(
            NoteKind::Skipped,
            "no Makefile targets had recipe lines — nothing to write to dish.toml",
        );
        return Ok(report);
    }

    // 5. Always surface the language=node-npm placeholder.
    report.push_note(
        NoteKind::Inferred,
        "review the `language` field in dish.toml — Makefile is language-agnostic, defaulted to \
         `node-npm` as a placeholder; set this to whatever fits your project.",
    );

    // 6. Emit dish.toml at the root.
    let dish_name = opts
        .root
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "dish".to_string());
    let dish_toml_path = opts.root.join("dish.toml");
    if dish_toml_path.exists() && !opts.force {
        report.push_note(
            NoteKind::Conflict,
            "dish.toml already exists — skipped (re-run with --force to overwrite)",
        );
    } else {
        let dish_body = render_dish_toml(&dish_name, &runnable);
        write_or_simulate(&dish_toml_path, &dish_body, opts.dry_run, &mut report)?;
    }

    // 7. Emit root bento.toml — placeholder shape, user fills in pins.
    let bento_toml_path = opts.root.join("bento.toml");
    if bento_toml_path.exists() && !opts.force {
        report.push_note(
            NoteKind::Conflict,
            "bento.toml already exists — skipped (re-run with --force to overwrite)",
        );
    } else {
        let bento_body = crate::init::render_bento_toml(&BTreeMap::new());
        write_or_simulate(&bento_toml_path, &bento_body, opts.dry_run, &mut report)?;
    }

    // 8. Emit bentos/prod.toml listing the single root dish.
    let prod_path = opts.root.join("bentos").join("prod.toml");
    if prod_path.exists() && !opts.force {
        report.push_note(
            NoteKind::Conflict,
            "bentos/prod.toml already exists — skipped (re-run with --force to overwrite)",
        );
    } else {
        if !opts.dry_run {
            fs::create_dir_all(prod_path.parent().unwrap()).context("creating bentos/")?;
        }
        let prod_body = crate::init::render_prod_toml(&[".".to_string()]);
        write_or_simulate(&prod_path, &prod_body, opts.dry_run, &mut report)?;
    }

    Ok(report)
}

// ── Parser ─────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct ParsedMakefile {
    targets: Vec<Target>,
    /// Pattern-rule headers we skipped (e.g. `%.o: %.c`).
    pattern_rules: Vec<String>,
    /// Names listed under `.PHONY:` directives.
    phony: Vec<String>,
    /// True iff any recipe contains `$(NAME)` or `${NAME}`.
    has_variable_expansion: bool,
}

#[derive(Debug)]
struct Target {
    name: String,
    prereqs: Vec<String>,
    /// Each recipe line, in source order, with the leading TAB stripped.
    recipe: Vec<String>,
}

/// Scan a Makefile body and extract the bits we care about. Best-effort:
/// we don't try to be a full Make parser, just recognise enough to
/// surface the shape of the build.
fn parse_makefile(body: &str) -> ParsedMakefile {
    let mut out = ParsedMakefile::default();
    // The currently-open target (None when we're between targets / in
    // the preamble). We push to this on TAB-indented lines, then flush
    // on the next blank line or non-recipe content.
    let mut current: Option<Target> = None;

    for raw in body.lines() {
        // Lines starting with TAB are recipe lines for the current target.
        if let Some(stripped) = raw.strip_prefix('\t') {
            // Recipe line — if we have an open target, append. Otherwise
            // this is a stray TAB line and we drop it.
            if let Some(t) = current.as_mut() {
                let line = stripped.trim_end_matches('\r');
                if !line.is_empty() {
                    if has_make_var_expansion(line) {
                        out.has_variable_expansion = true;
                    }
                    t.recipe.push(line.to_string());
                }
            }
            continue;
        }

        // Non-tab line — close any open target and process the line.
        if let Some(t) = current.take() {
            out.targets.push(t);
        }

        let line = raw.trim_end_matches('\r');
        let trimmed = line.trim();

        // Blank line — ends the current target (already closed above).
        if trimmed.is_empty() {
            continue;
        }
        // Comment line — skip.
        if trimmed.starts_with('#') {
            continue;
        }

        // .PHONY: a b c
        if let Some(rest) = trimmed.strip_prefix(".PHONY:") {
            for name in rest.split_whitespace() {
                out.phony.push(name.to_string());
            }
            continue;
        }

        // Variable assignment (`VAR := …`, `VAR = …`, `VAR ?= …`,
        // `VAR += …`, `export VAR := …`). Detected by looking for an
        // assignment operator BEFORE any colon. If we see one, skip.
        if is_variable_assignment(trimmed) {
            continue;
        }

        // Other directives we don't handle: include, ifeq/ifneq/else/endif,
        // define/endef, vpath, override. Skip silently — they don't
        // produce targets.
        if is_make_directive(trimmed) {
            continue;
        }

        // Target line: `name [name2 ...]: [prereq ...]`. The colon must
        // not be part of `:=`.
        if let Some((header, prereq_str)) = split_target_line(trimmed) {
            let names: Vec<&str> = header.split_whitespace().collect();
            for name in names {
                // Pattern rules contain `%`. We surface them as a note
                // and don't open a target for them — but we still let
                // the recipe lines below them be ignored (the `current`
                // is None so they fall through).
                if name.contains('%') {
                    out.pattern_rules.push(format!(
                        "{name}:{}",
                        if prereq_str.is_empty() {
                            String::new()
                        } else {
                            format!(" {prereq_str}")
                        }
                    ));
                    continue;
                }
                let prereqs: Vec<String> = prereq_str
                    .split_whitespace()
                    .map(|s| s.to_string())
                    .collect();
                // If multiple targets share one header (e.g. `a b: dep`),
                // we open a fresh target for each — but only the LAST
                // one keeps the recipe (Make's actual semantics is each
                // gets the recipe; we approximate by using the last,
                // which matches the typical "alias" idiom). Push the
                // earlier ones with empty recipes immediately.
                let target = Target {
                    name: name.to_string(),
                    prereqs: prereqs.clone(),
                    recipe: Vec::new(),
                };
                if let Some(prev) = current.replace(target) {
                    out.targets.push(prev);
                }
            }
            continue;
        }

        // Anything else — just drop. Best-effort.
    }

    if let Some(t) = current.take() {
        out.targets.push(t);
    }

    out
}

/// `true` if the line is a Make variable assignment (`VAR = …`,
/// `VAR := …`, `VAR ?= …`, `VAR += …`, optionally prefixed by `export`).
fn is_variable_assignment(line: &str) -> bool {
    let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
    // Find the first `=` and check that the char before it is one of
    // `:`, `?`, `+`, or none (plain `=`). And ensure no colon appears
    // before the `=` that isn't part of `:=`.
    for (i, c) in line.char_indices() {
        if c == '=' {
            // Look at what's just before.
            let before = &line[..i];
            let trimmed = before.trim_end();
            if trimmed.is_empty() {
                return false;
            }
            let last = trimmed.chars().last().unwrap();
            // Plain assignment, conditional, append, or simply-expanded.
            if last == ':' || last == '?' || last == '+' {
                // Make sure the lhs is a single identifier (no spaces
                // in the trimmed prefix minus the operator char).
                let lhs_end = trimmed.len() - last.len_utf8();
                let lhs = trimmed[..lhs_end].trim();
                return is_identifier(lhs);
            } else {
                return is_identifier(trimmed);
            }
        }
        if c == ':' {
            // Could be `:=` (handled above on the next iteration) or a
            // target separator. Peek next char.
            let rest = &line[i + 1..];
            if rest.starts_with('=') {
                continue;
            }
            return false;
        }
    }
    false
}

fn is_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

fn is_make_directive(line: &str) -> bool {
    const DIRECTIVES: &[&str] = &[
        "include ",
        "-include ",
        "sinclude ",
        "ifeq ",
        "ifeq(",
        "ifneq ",
        "ifneq(",
        "ifdef ",
        "ifndef ",
        "else",
        "endif",
        "define ",
        "endef",
        "vpath ",
        "override ",
        "unexport ",
        "export ", // bare `export VAR` (no `=`) lands here
    ];
    DIRECTIVES
        .iter()
        .any(|d| line == d.trim_end() || line.starts_with(d))
}

/// Split a target header line on the FIRST `:` that isn't part of `:=`.
/// Returns `(target_names_str, prereqs_str)`. Returns `None` if no
/// target separator is present.
fn split_target_line(line: &str) -> Option<(&str, &str)> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b':' {
            // Skip `::` (double-colon rules — same as `:` for our purposes,
            // we step past the second colon).
            if i + 1 < bytes.len() && bytes[i + 1] == b'=' {
                // `:=` is an assignment; we shouldn't be parsing this as
                // a target line. Bail out.
                return None;
            }
            let mut prereq_start = i + 1;
            if prereq_start < bytes.len() && bytes[prereq_start] == b':' {
                prereq_start += 1;
            }
            let header = line[..i].trim();
            let prereqs = line[prereq_start..].trim();
            if header.is_empty() {
                return None;
            }
            return Some((header, prereqs));
        }
        i += 1;
    }
    None
}

fn has_make_var_expansion(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'$' && (bytes[i + 1] == b'(' || bytes[i + 1] == b'{') {
            return true;
        }
        i += 1;
    }
    false
}

fn recipe_uses_automatic_var(recipe: &[String]) -> bool {
    const AUTO: &[&str] = &[
        "$@", "$<", "$^", "$?", "$+", "$|", "$*", "$%", "$(@D)", "$(@F)", "$(<D)", "$(<F)",
        "$(^D)", "$(^F)", "$(*D)", "$(*F)",
    ];
    recipe
        .iter()
        .any(|line| AUTO.iter().any(|v| line.contains(v)))
}

// ── dish.toml renderer ─────────────────────────────────────────────

/// Choice: multi-line recipes are joined with ` && ` so the whole task
/// fails fast on the first non-zero exit. This matches Make's own
/// per-line strict-mode semantics (`set -e` is implied in most modern
/// Makefiles via `.ONESHELL` + `SHELL := bash -e`, and short-circuiting
/// is how we'd want bento to behave anyway).
fn render_dish_toml(dish_name: &str, runnable: &[&Target]) -> String {
    let mut body = format!(
        "name = \"{dish_name}\"\n\
         language = \"node-npm\"\n\
         \n\
         # Migrated from Makefile. Each [tasks.<name>] mirrors a Makefile\n\
         # target — recipe lines are joined with ` && ` so the task\n\
         # fails fast on the first non-zero exit.\n\
         #\n\
         # Review the `language` field — Makefile is language-agnostic;\n\
         # `node-npm` is just a placeholder.\n",
    );

    for t in runnable {
        body.push('\n');
        body.push_str(&format!("[tasks.{}]\n", toml_table_key(&t.name)));
        let joined = t.recipe.join(" && ");
        body.push_str(&format!("run = {}\n", toml_basic_string(&joined)));
    }

    body
}

// ── Helpers ────────────────────────────────────────────────────────

fn write_or_simulate(
    path: &Path,
    body: &str,
    dry_run: bool,
    report: &mut MigrationReport,
) -> Result<()> {
    if !dry_run {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
    }
    report.push_file(path.to_path_buf(), body.len());
    Ok(())
}

// ── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();
        assert!(report.applied);

        let dish = std::fs::read_to_string(tmp.path().join("dish.toml")).unwrap();
        assert!(dish.contains("[tasks.build]"));
        assert!(dish.contains(r#"run = "cargo build""#));
        assert!(dish.contains("[tasks.test]"));
        assert!(dish.contains(r#"run = "cargo test""#));
        assert!(dish.contains("[tasks.clean]"));
        assert!(dish.contains(r#"run = "rm -rf target""#));

        // bento.toml + bentos/prod.toml emitted too.
        assert!(tmp.path().join("bento.toml").exists());
        let prod = std::fs::read_to_string(tmp.path().join("bentos/prod.toml")).unwrap();
        assert!(prod.contains("\".\""));
    }

    #[test]
    fn refuses_to_overwrite_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        write_makefile(&tmp, "build:\n\techo built\n");
        std::fs::write(tmp.path().join("dish.toml"), "name = \"existing\"\n").unwrap();
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();
        assert!(report.has_conflicts());
        // dish.toml stays untouched.
        let body = std::fs::read_to_string(tmp.path().join("dish.toml")).unwrap();
        assert_eq!(body, "name = \"existing\"\n");
    }

    #[test]
    fn dry_run_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        write_makefile(&tmp, "build:\n\techo built\n");
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: true,
            force: false,
        })
        .unwrap();
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
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: true,
            force: false,
        })
        .unwrap();
        let kinds: Vec<_> = report.notes.iter().map(|n| n.kind).collect();
        assert!(
            kinds.contains(&NoteKind::Skipped),
            "pattern rule should produce a Skipped note; got {:?}",
            report.notes
        );
        let has_pattern_msg = report
            .notes
            .iter()
            .any(|n| n.kind == NoteKind::Skipped && n.message.contains("%.o"));
        assert!(has_pattern_msg, "Skipped note should reference %.o pattern");
    }

    #[test]
    fn surfaces_phony_targets_in_notes() {
        let tmp = tempfile::tempdir().unwrap();
        write_makefile(
            &tmp,
            ".PHONY: clean test\n\nclean:\n\trm -rf target\n\ntest:\n\tcargo test\n",
        );
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: true,
            force: false,
        })
        .unwrap();
        let phony_msg = report
            .notes
            .iter()
            .find(|n| n.message.contains(".PHONY"))
            .expect("expected a note mentioning .PHONY");
        assert!(phony_msg.message.contains("clean"));
        assert!(phony_msg.message.contains("test"));
    }

    #[test]
    fn parses_multiline_recipe() {
        let tmp = tempfile::tempdir().unwrap();
        write_makefile(
            &tmp,
            "release:\n\tcargo build --release\n\tstrip target/release/foo\n\ttar -czf foo.tgz target/release/foo\n",
        );
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();
        let _ = report;
        let dish = std::fs::read_to_string(tmp.path().join("dish.toml")).unwrap();
        assert!(dish.contains("cargo build --release"));
        assert!(dish.contains("strip target/release/foo"));
        assert!(dish.contains("tar -czf foo.tgz target/release/foo"));
        // All three lines should be in a single `run = "..."` joined by ` && `.
        assert!(dish.contains("cargo build --release && strip target/release/foo && tar -czf foo.tgz target/release/foo"));
    }

    #[test]
    fn flags_automatic_variables() {
        let tmp = tempfile::tempdir().unwrap();
        write_makefile(&tmp, "copy: src.txt\n\tcp $< $@\n");
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: true,
            force: false,
        })
        .unwrap();
        let auto_note = report
            .notes
            .iter()
            .find(|n| n.kind == NoteKind::Inferred && n.message.contains("automatic variables"))
            .expect("expected an Inferred note about automatic variables");
        assert!(auto_note.message.contains("copy"));
    }

    #[test]
    fn skips_variable_assignments() {
        let tmp = tempfile::tempdir().unwrap();
        write_makefile(
            &tmp,
            "CC := gcc\nCFLAGS = -O2 -Wall\nVERBOSE ?= 0\nWARNINGS += -Wextra\n\nbuild:\n\t$(CC) $(CFLAGS) -o foo foo.c\n",
        );
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();
        let _ = report;
        let dish = std::fs::read_to_string(tmp.path().join("dish.toml")).unwrap();
        assert!(dish.contains("[tasks.build]"));
        assert!(dish.contains("$(CC)"));
        // No tasks generated for assignment lines.
        assert!(!dish.contains("[tasks.CC]"));
        assert!(!dish.contains("[tasks.CFLAGS]"));
        assert!(!dish.contains("[tasks.VERBOSE]"));
        assert!(!dish.contains("[tasks.WARNINGS]"));
    }

    // ── extra parser-level coverage ────────────────────────────────

    #[test]
    fn detects_make_variable_expansion_note() {
        let tmp = tempfile::tempdir().unwrap();
        write_makefile(&tmp, "build:\n\t$(CC) -o foo foo.c\n");
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: true,
            force: false,
        })
        .unwrap();
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.message.contains("variable expansions")),
            "expected a variable-expansion note"
        );
    }

    #[test]
    fn target_with_prereqs_emits_inferred_note() {
        let tmp = tempfile::tempdir().unwrap();
        write_makefile(
            &tmp,
            "clean:\n\trm -rf out\n\nbuild: clean\n\tmkdir out && echo built > out/log\n",
        );
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: true,
            force: false,
        })
        .unwrap();
        let prereq_note = report
            .notes
            .iter()
            .find(|n| n.kind == NoteKind::Inferred && n.message.contains("prerequisites"))
            .expect("expected an Inferred note about prerequisites");
        assert!(prereq_note.message.contains("build"));
    }

    #[test]
    fn missing_makefile_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let err = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Makefile"));
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        write_makefile(
            &tmp,
            "# top-level comment\n\n# another\nbuild:\n\t# in-recipe comments are kept verbatim\n\techo built\n",
        );
        let report = run(Options {
            root: tmp.path().to_path_buf(),
            dry_run: false,
            force: false,
        })
        .unwrap();
        let _ = report;
        let dish = std::fs::read_to_string(tmp.path().join("dish.toml")).unwrap();
        assert!(dish.contains("[tasks.build]"));
        // Top-level comment lines should NOT become tasks.
        assert!(!dish.contains("[tasks.\"#\"]"));
    }

    #[test]
    fn split_target_line_handles_double_colon() {
        let (h, p) = split_target_line("foo:: bar baz").unwrap();
        assert_eq!(h, "foo");
        assert_eq!(p, "bar baz");
    }

    #[test]
    fn split_target_line_rejects_assignment() {
        assert!(split_target_line("CC := gcc").is_none());
    }

    #[test]
    fn is_variable_assignment_recognises_all_operators() {
        assert!(is_variable_assignment("CC := gcc"));
        assert!(is_variable_assignment("CC = gcc"));
        assert!(is_variable_assignment("CC ?= gcc"));
        assert!(is_variable_assignment("CC += gcc"));
        assert!(is_variable_assignment("export CC := gcc"));
        assert!(!is_variable_assignment("build: dep"));
        assert!(!is_variable_assignment("build:"));
    }
}
