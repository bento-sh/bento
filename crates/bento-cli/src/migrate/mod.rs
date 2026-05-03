//! `bento migrate <tool>` — convert a competing monorepo tool's
//! workspace config into bento config.
//!
//! Each migrator reads the source tool's manifests (`turbo.json`,
//! `nx.json`, `moon/workspace.yml`, …), walks the package layout,
//! and emits the equivalent bento config: workspace `bento.toml`,
//! per-package `dish.toml`s, and a starter `bentos/prod.toml`.
//!
//! Migrators are intentionally non-destructive: by default they refuse
//! to overwrite any existing bento file. `--force` opts in to clobber.
//! `--dry-run` prints the report without touching the filesystem.
//!
//! The output is a *starting point* the user reviews and tweaks — not
//! a perfect 1:1 translation. Notes are included in the report for
//! anything the migrator couldn't faithfully translate (per-package
//! overrides, persistent dev tasks, custom cache settings).

use std::fmt;
use std::path::PathBuf;

pub mod lerna;
pub mod make;
pub mod moon;
pub mod nx;
pub mod rush;
pub mod turbo;

/// Common report shape across all migrators. Printed to the user (with
/// human formatting) and serialised to `--json` mode.
#[derive(Debug, Default, serde::Serialize)]
pub struct MigrationReport {
    /// Files the migrator wrote (or *would* have written under `--dry-run`).
    pub files_written: Vec<WrittenFile>,
    /// Things the migrator skipped or couldn't translate. Each note has
    /// a stable kind so agents can filter, plus a human message.
    pub notes: Vec<MigrationNote>,
    /// Did we actually touch the filesystem? `false` under `--dry-run`.
    pub applied: bool,
}

#[derive(Debug, serde::Serialize)]
pub struct WrittenFile {
    pub path: PathBuf,
    pub bytes: usize,
}

#[derive(Debug, serde::Serialize)]
pub struct MigrationNote {
    pub kind: NoteKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    /// We chose not to translate something — usually because the source
    /// concept doesn't exist in bento (turbo's `cache: false`, persistent
    /// dev tasks).
    Skipped,
    /// A heuristic guess that the user should review (e.g. inferring
    /// `outputs` from a missing turbo declaration).
    Inferred,
    /// Source feature we recognise but haven't implemented yet.
    NotYetImplemented,
    /// Refused to overwrite an existing file (the user must `--force`).
    Conflict,
}

impl fmt::Display for NoteKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoteKind::Skipped => f.write_str("skipped"),
            NoteKind::Inferred => f.write_str("inferred"),
            NoteKind::NotYetImplemented => f.write_str("not-yet-impl"),
            NoteKind::Conflict => f.write_str("conflict"),
        }
    }
}

impl MigrationReport {
    pub fn push_file(&mut self, path: PathBuf, bytes: usize) {
        self.files_written.push(WrittenFile { path, bytes });
    }
    pub fn push_note(&mut self, kind: NoteKind, message: impl Into<String>) {
        self.notes.push(MigrationNote {
            kind,
            message: message.into(),
        });
    }
    /// True iff the migrator hit any conflict the user must resolve.
    pub fn has_conflicts(&self) -> bool {
        self.notes.iter().any(|n| n.kind == NoteKind::Conflict)
    }
}

pub fn print_human(report: &MigrationReport) {
    use crate::style;
    if report.files_written.is_empty() && !report.applied {
        println!("{}", style::dim("(nothing to write)"));
    }
    for f in &report.files_written {
        let prefix = if report.applied {
            style::green("✓ wrote ")
        } else {
            style::dim("· would write ")
        };
        println!("{prefix}{} ({} bytes)", f.path.display(), f.bytes);
    }
    if !report.notes.is_empty() {
        println!();
        for n in &report.notes {
            let tag = match n.kind {
                NoteKind::Conflict => style::red(&format!("[{}]", n.kind)),
                NoteKind::Skipped => style::dim(&format!("[{}]", n.kind)),
                NoteKind::Inferred => style::yellow(&format!("[{}]", n.kind)),
                NoteKind::NotYetImplemented => style::yellow(&format!("[{}]", n.kind)),
            };
            println!("  {tag}  {msg}", msg = n.message);
        }
    }
}
