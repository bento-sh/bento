//! Shared contract for remote-cache backends.
//!
//! `bento` speaks two remote protocols today:
//!
//! - **S3-compatible** (`s3://bucket/prefix`) — HEAD/GET/PUT against any
//!   AWS-signed object store. Self-hosted friendly: R2, MinIO, Backblaze.
//!   Lives in [`crate::remote::S3Remote`].
//! - **Bento Bearer** (`bento://host[/prefix]`) — HTTP + `Authorization:
//!   Bearer <JWT>` against `cache.bento.build` or any server implementing
//!   the same wire protocol. Lives in [`crate::remote_bearer::BearerRemote`].
//!
//! Both implement [`RemoteCache`]; the executor holds
//! `Option<Box<dyn RemoteCache>>` and doesn't care which backend it got.
//!
//! # Best-effort semantics
//!
//! Every method keeps the "remote failures never fail the build" contract:
//!
//! - `has` returns `false` on any non-200 response or transport error.
//! - `get` returns `Ok(false)` on 404 / transport failure / body-read
//!   failure; it only returns `Err` for local I/O problems writing the
//!   downloaded bundle to disk.
//! - `put` returns `Err` on upstream failure so the executor can log +
//!   swallow (the local cache is still authoritative).

use std::path::Path;

use anyhow::Result;

use crate::key::CacheKey;

/// Uniform handle over a remote content-addressable cache.
///
/// Implementors are used from the executor hot path: `has` is called per
/// cache-eligible task, `get` on a probable hit, `put` after a fresh
/// build finishes. Trait methods are intentionally synchronous — each
/// implementor owns whatever runtime / HTTP client it needs internally.
pub trait RemoteCache: Send + Sync {
    /// Does the remote hold a bundle for `key`?
    ///
    /// Transport errors and non-200 responses must be treated as misses
    /// (return `false`) to preserve the best-effort contract.
    fn has(&self, key: &CacheKey) -> bool;

    /// Download the bundle for `key` to `dest`. Returns `Ok(true)` on a
    /// successful write, `Ok(false)` on miss / transport failure / body
    /// error. Only `Err` for a local-filesystem write problem.
    fn get(&self, key: &CacheKey, dest: &Path) -> Result<bool>;

    /// Upload `bundle_path` as the bundle for `key`. Errors bubble up so
    /// the executor can log + keep the local cache.
    fn put(&self, key: &CacheKey, bundle_path: &Path) -> Result<()>;

    /// Human-readable URL used in logs and `bento cache` output.
    fn display_url(&self) -> &str;
}

/// Parse a `[cache]` `remote` URL's scheme. Lightweight wrapper so
/// [`build_remote`] can dispatch without duplicating string munging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteScheme {
    /// `s3://…` — AWS-signed object store.
    S3,
    /// `bento://…` — bearer-auth HTTP cache.
    Bento,
}

impl RemoteScheme {
    /// Detect the scheme from a remote URL, returning `None` if the URL
    /// has no known prefix.
    pub fn detect(url: &str) -> Option<Self> {
        if url.starts_with("s3://") {
            Some(Self::S3)
        } else if url.starts_with("bento://") {
            Some(Self::Bento)
        } else {
            None
        }
    }
}

/// Build the right [`RemoteCache`] from the parsed config fields.
///
/// Inputs come verbatim from the `[cache]` block in `bento.toml`:
/// `url` is the `remote` value; `region`/`endpoint` are S3-only;
/// `token` is the bearer JWT (pre-resolved from `remote_token_env`).
///
/// Errors surface the first parse/build failure — callers typically log
/// and disable the remote tier rather than abort the whole run.
pub fn build_remote(
    url: &str,
    region: Option<&str>,
    endpoint: Option<&str>,
    token: Option<&str>,
) -> Result<Box<dyn RemoteCache>> {
    match RemoteScheme::detect(url) {
        Some(RemoteScheme::S3) => {
            let r = crate::remote::S3Remote::new(url, region.unwrap_or("us-east-1"), endpoint)?;
            Ok(Box::new(r))
        }
        Some(RemoteScheme::Bento) => {
            let token = token.ok_or_else(|| {
                anyhow::anyhow!(
                    "bento:// remote cache requires a token — \
                     set `remote_token_env = \"BENTO_CACHE_TOKEN\"` in \
                     [cache] and export that env var with your JWT"
                )
            })?;
            let r = crate::remote_bearer::BearerRemote::new(url, token.to_string())?;
            Ok(Box::new(r))
        }
        None => anyhow::bail!(
            "unrecognised remote cache URL scheme: {url} (expected s3:// or bento://)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_schemes() {
        assert_eq!(RemoteScheme::detect("s3://b/p"), Some(RemoteScheme::S3));
        assert_eq!(
            RemoteScheme::detect("bento://cache.bento.build"),
            Some(RemoteScheme::Bento)
        );
        assert_eq!(RemoteScheme::detect("https://x"), None);
        assert_eq!(RemoteScheme::detect(""), None);
    }

    #[test]
    fn build_rejects_unknown_scheme() {
        let err = build_remote("gs://bucket", None, None, None)
            .map(|_| ())
            .unwrap_err();
        assert!(err.to_string().contains("unrecognised"), "got: {err}");
    }

    #[test]
    fn build_bento_requires_token() {
        let err = build_remote("bento://cache.bento.build", None, None, None)
            .map(|_| ())
            .unwrap_err();
        assert!(err.to_string().contains("requires a token"), "got: {err}");
    }
}
