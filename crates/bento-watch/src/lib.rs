//! File watcher for `bento serve` and `bento dev`.
//!
//! Wraps [`notify::RecommendedWatcher`] with a glob filter + debounce so
//! consumers get a single "something changed" tick per burst rather than
//! the dozens of events a single save tends to fan out into (editor
//! temp-files, FS sync, backup writes, …).

use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};

/// Stream of coalesced change events for a single dish.
///
/// `DishWatcher::next` blocks until the debounce window has elapsed after
/// the first matching file event, then returns the set of changed paths.
/// Returns `None` only on watcher shutdown.
pub struct DishWatcher {
    _watcher: RecommendedWatcher,
    rx: Receiver<notify::Result<Event>>,
    matcher: GlobSet,
    root: PathBuf,
    debounce: Duration,
}

#[derive(Debug, Clone)]
pub struct ChangeBatch {
    pub paths: Vec<PathBuf>,
}

impl DishWatcher {
    /// Start watching `dish_dir` recursively, filtering events to files
    /// matching any of `globs` (relative to `dish_dir`).
    pub fn new(dish_dir: &Path, globs: &[String], debounce: Duration) -> Result<Self> {
        let matcher = build_matcher(globs)?;
        let (tx, rx): (Sender<notify::Result<Event>>, _) = channel();
        let mut watcher = notify::recommended_watcher(tx).context("creating file watcher")?;
        watcher
            .watch(dish_dir, RecursiveMode::Recursive)
            .with_context(|| format!("watching {}", dish_dir.display()))?;

        Ok(Self {
            _watcher: watcher,
            rx,
            matcher,
            root: dish_dir.to_path_buf(),
            debounce,
        })
    }

    /// Block until a matching change is observed, then drain for
    /// `debounce` so editor-save fanout collapses into one tick. Returns
    /// `None` only if the watcher's sender has been dropped (shutdown).
    pub fn next_batch(&self) -> Option<ChangeBatch> {
        let first = loop {
            let res = self.rx.recv().ok()?;
            if let Some(paths) = self.extract_relevant_paths(res) {
                break paths;
            }
        };

        // Now drain for `debounce`, accumulating any further hits that
        // arrive in the window.
        let deadline = Instant::now() + self.debounce;
        let mut all: Vec<PathBuf> = first;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .unwrap_or_default();
            if remaining.is_zero() {
                break;
            }
            match self.rx.recv_timeout(remaining) {
                Ok(res) => {
                    if let Some(mut more) = self.extract_relevant_paths(res) {
                        all.append(&mut more);
                    }
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }

        all.sort();
        all.dedup();
        Some(ChangeBatch { paths: all })
    }

    fn extract_relevant_paths(&self, res: notify::Result<Event>) -> Option<Vec<PathBuf>> {
        let event = match res {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("file-watch error: {e}");
                return None;
            }
        };
        if !is_content_event(&event.kind) {
            return None;
        }
        let mut matched: Vec<PathBuf> = Vec::new();
        for path in &event.paths {
            let Ok(rel) = path.strip_prefix(&self.root) else {
                continue;
            };
            if self.matcher.is_match(rel) {
                matched.push(path.clone());
            }
        }
        if matched.is_empty() {
            None
        } else {
            Some(matched)
        }
    }
}

fn build_matcher(globs: &[String]) -> Result<GlobSet> {
    if globs.is_empty() {
        // A dish with no explicit inputs should watch everything under
        // its own tree — not ideal for noisy dirs, but a reasonable
        // default and easy for users to tighten later.
        let mut b = GlobSetBuilder::new();
        b.add(Glob::new("**/*").context("compiling default glob")?);
        return Ok(b.build()?);
    }
    let mut b = GlobSetBuilder::new();
    for g in globs {
        b.add(Glob::new(g).with_context(|| format!("compiling watch glob `{g}`"))?);
    }
    Ok(b.build()?)
}

/// Which notify event kinds count as "the user saved something".
/// Metadata-only events (`Access`) fire constantly under the hood and
/// would make the dev loop feel laggy, so we filter them out.
fn is_content_event(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn new_compiles_and_builds_default_matcher_for_empty_globs() {
        let tmp = tempfile::tempdir().unwrap();
        let w = DishWatcher::new(tmp.path(), &[], Duration::from_millis(50)).unwrap();
        // Default matcher should match any relative path.
        assert!(w.matcher.is_match("src/foo.rs"));
        assert!(w.matcher.is_match("deeply/nested/thing.js"));
    }

    #[test]
    fn build_matcher_respects_globs() {
        let m = build_matcher(&["src/**/*.rs".into(), "Cargo.toml".into()]).unwrap();
        assert!(m.is_match("src/main.rs"));
        assert!(m.is_match("Cargo.toml"));
        assert!(!m.is_match("node_modules/x.js"));
    }

    #[test]
    fn is_content_event_filters_access_kind() {
        use notify::event::{AccessKind, ModifyKind};
        assert!(!is_content_event(&EventKind::Access(AccessKind::Read)));
        assert!(is_content_event(&EventKind::Modify(ModifyKind::Any)));
    }

    // A real IO test would be flaky in CI; `notify`'s own test suite
    // exercises the underlying watcher.  We rely on those, and cover
    // the filter/matcher logic here.
}
