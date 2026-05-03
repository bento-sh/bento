//! Toolchain-version probe helper.
//!
//! Each adapter's `resolved_toolchain_fingerprint` wants to answer the
//! question "what's the actual installed version of this tool?" — usually
//! by running `<tool> --version` and capturing stdout. That subprocess
//! cost is the same for every dish that uses the adapter, so we memoise
//! it per `(program, args)` pair for the lifetime of the process.
//!
//! Returns `None` when the probe command is not on PATH or exits
//! non-zero; compute_key then simply doesn't mix in a fingerprint.

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Mutex, OnceLock};

fn cache() -> &'static Mutex<HashMap<String, Option<String>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Option<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Run `program args…`, trim, cache, and return stdout (or None on
/// failure). Cache key is the joined command string.
pub fn memoised(program: &str, args: &[&str]) -> Option<String> {
    let key = if args.is_empty() {
        program.to_string()
    } else {
        format!("{program} {}", args.join(" "))
    };

    // Fast path: already probed.
    if let Ok(guard) = cache().lock() {
        if let Some(hit) = guard.get(&key) {
            return hit.clone();
        }
    }

    let result = run(program, args);

    if let Ok(mut guard) = cache().lock() {
        guard.insert(key, result.clone());
    }
    result
}

fn run(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
pub(crate) fn _clear_cache_for_tests() {
    if let Ok(mut guard) = cache().lock() {
        guard.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_program_returns_none() {
        let result = memoised("this-binary-should-not-exist-xyz", &["--version"]);
        assert!(result.is_none());
    }

    #[test]
    fn memoised_returns_same_value_on_second_call() {
        // We can't trust any specific binary to exist on the test host,
        // so exercise the cache via two calls and check they agree.
        let first = memoised("echo", &["hello"]);
        let second = memoised("echo", &["hello"]);
        assert_eq!(first, second);
        assert_eq!(first.as_deref(), Some("hello"));
    }
}
