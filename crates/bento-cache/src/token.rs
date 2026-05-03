//! Cache-token resolver + sink shared between `bento login` (writes)
//! and `bento build|ci|…` (reads).
//!
//! Resolution order for reads:
//!
//!   1. `$<env_var_name>` — explicit environment variable, usually
//!      `BENTO_CACHE_TOKEN`. Winning here lets CI keep working with a
//!      secret injected by the runner and matches the precedence
//!      convention of AWS/gcloud/Anthropic CLIs ("an env var I set
//!      intentionally should override implicit state").
//!   2. OS keychain — entry `("bento", "cache-token")`. Written by
//!      `bento login` on the first interactive session.
//!   3. `~/.bento/credentials` — plain-text JWT, mode 0600. Used when
//!      no keychain backend is available (SSH sessions, headless
//!      containers, users who declined the OS password prompt).
//!
//! Writes use the same precedence in reverse: try keychain first, fall
//! back to the 0600 file. Callers get a [`TokenSink`] back so they can
//! report *where* the token landed ("Token stored in keychain" vs
//! "Token stored in ~/.bento/credentials").

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Service name used for the keychain entry. The pair is stable: a
/// second `bento login` overwrites the existing entry at the same
/// address. Changing these strings is a breaking change — existing
/// installs will appear logged-out.
const KEYRING_SERVICE: &str = "bento";
const KEYRING_USER: &str = "cache-token";

/// Which storage path actually produced / consumed the JWT. Returned
/// from the write helpers so CLI UX can say "stored in keychain" vs
/// "stored in ~/.bento/credentials (0600)".
#[derive(Debug, Clone)]
pub enum TokenSink {
    Keychain,
    File(PathBuf),
}

/// Read the configured cache token for `bento://` remotes.
///
/// `env_var_name` is what `[cache].remote_token_env` resolved to —
/// typically `BENTO_CACHE_TOKEN` but configurable per-repo so two
/// overlapping workspaces on one machine can use distinct tokens
/// without a shared keychain entry.
///
/// Returns `None` only when every tier is empty. The remote-cache
/// client then disables the remote tier with a warning; callers don't
/// need to distinguish "no token configured" from "keychain read
/// failed" — every source has had a fair turn.
pub fn resolve_cache_token(env_var_name: &str) -> Option<String> {
    if let Ok(v) = std::env::var(env_var_name) {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    if let Some(t) = keychain_read() {
        return Some(t);
    }
    file_fallback_read()
}

/// Persist `jwt` to the best available sink. Tries keychain first;
/// falls back to the file when the keyring backend errors. Returning
/// the [`TokenSink`] lets the caller print the right "stored in …"
/// line without re-probing.
pub fn store_cache_token(jwt: &str) -> Result<TokenSink> {
    match keychain_write(jwt) {
        Ok(()) => Ok(TokenSink::Keychain),
        Err(e) => {
            tracing::debug!("keychain write failed ({e:#}), falling back to file");
            let path = file_fallback_write(jwt)
                .context("writing ~/.bento/credentials after keychain failure")?;
            Ok(TokenSink::File(path))
        }
    }
}

/// Path used by the file-fallback sink. `None` only when `$HOME` is
/// unset and `dirs` can't resolve a home directory — very rare in
/// practice but possible under certain CI containers.
pub fn file_fallback_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".bento").join("credentials"))
}

fn keychain_read() -> Option<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).ok()?;
    match entry.get_password() {
        Ok(s) if !s.is_empty() => Some(s),
        Ok(_) => None,
        Err(keyring::Error::NoEntry) => None,
        Err(e) => {
            tracing::debug!("keyring read failed: {e}");
            None
        }
    }
}

fn keychain_write(jwt: &str) -> Result<()> {
    let entry =
        keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).context("constructing keyring entry")?;
    entry.set_password(jwt).context("writing JWT to keyring")?;
    Ok(())
}

fn file_fallback_read() -> Option<String> {
    let path = file_fallback_path()?;
    let raw = std::fs::read_to_string(&path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn file_fallback_write(jwt: &str) -> Result<PathBuf> {
    let path = file_fallback_path()
        .ok_or_else(|| anyhow::anyhow!("can't resolve HOME for credentials fallback"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("opening {} (0600)", path.display()))?;
        f.write_all(jwt.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, jwt).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(path)
}
