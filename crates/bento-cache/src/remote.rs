//! S3-compatible remote cache backed by the `object_store` crate.
//!
//! Config:
//!
//! ```toml
//! [cache]
//! remote          = "s3://bucket/optional/prefix"
//! remote_region   = "us-east-1"          # required
//! remote_endpoint = "https://…"          # optional — R2/MinIO/Backblaze
//! ```
//!
//! Credentials come from the standard AWS environment chain:
//! `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, optional
//! `AWS_SESSION_TOKEN`. This covers AWS S3, Cloudflare R2 (with
//! their S3-compat keys), MinIO, Backblaze B2, and most S3-API
//! services.
//!
//! # Failure semantics
//!
//! Remote cache is strictly best-effort:
//! - A remote miss or transport error never fails the build; we fall
//!   back to the local cache / fresh execution.
//! - A remote put failure is logged and swallowed; the local cache is
//!   still authoritative.

use std::path::Path;

use anyhow::{Context, Result};
use bytes::Bytes;
use object_store::aws::AmazonS3Builder;
use object_store::{ObjectStore, PutPayload};

use crate::key::CacheKey;
use crate::remote_api::RemoteCache;

/// Handle to an S3-compatible cache bucket. Internally owns a small
/// tokio runtime for `object_store`'s async API; the public surface is
/// fully synchronous so the executor doesn't need to change.
pub struct S3Remote {
    store: Box<dyn ObjectStore>,
    prefix: String,
    display_url: String,
    rt: tokio::runtime::Runtime,
}

impl S3Remote {
    /// Build an S3 remote from the parsed config fields.
    ///
    /// `remote_url` is `s3://<bucket>/<optional/prefix>` (the
    /// `[cache] remote` value). `region` is required. `endpoint` is
    /// only needed for non-AWS services.
    pub fn new(remote_url: &str, region: &str, endpoint: Option<&str>) -> Result<Self> {
        let (bucket, prefix) = parse_s3_url(remote_url)?;
        let display_url = remote_url.to_string();

        let mut builder = AmazonS3Builder::from_env()
            .with_bucket_name(&bucket)
            .with_region(region);

        if let Some(ep) = endpoint {
            builder = builder
                .with_endpoint(ep)
                .with_virtual_hosted_style_request(false);
        }

        let store = builder
            .build()
            .with_context(|| format!("building S3 client for {remote_url}"))?;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("creating tokio runtime for S3 remote cache")?;

        Ok(Self {
            store: Box::new(store),
            prefix,
            display_url,
            rt,
        })
    }

    fn object_path(&self, key: &CacheKey) -> object_store::path::Path {
        let name = format!("{}.tar", key.as_hex());
        if self.prefix.is_empty() {
            object_store::path::Path::from(name)
        } else {
            object_store::path::Path::from(format!("{}/{name}", self.prefix))
        }
    }
}

impl RemoteCache for S3Remote {
    fn has(&self, key: &CacheKey) -> bool {
        let path = self.object_path(key);
        match self.rt.block_on(self.store.head(&path)) {
            Ok(_) => true,
            Err(object_store::Error::NotFound { .. }) => false,
            Err(e) => {
                tracing::debug!("remote HEAD {path} failed: {e}");
                false
            }
        }
    }

    fn get(&self, key: &CacheKey, dest: &Path) -> Result<bool> {
        let path = self.object_path(key);
        let result = match self.rt.block_on(self.store.get(&path)) {
            Ok(r) => r,
            Err(object_store::Error::NotFound { .. }) => return Ok(false),
            Err(e) => {
                tracing::warn!("remote GET failed: {e}");
                return Ok(false);
            }
        };

        let data: Bytes = match self.rt.block_on(result.bytes()) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("remote GET read body failed: {e}");
                return Ok(false);
            }
        };

        let tmp = tmp_sibling(dest);
        std::fs::write(&tmp, &data)
            .with_context(|| format!("writing remote bundle to {}", tmp.display()))?;
        std::fs::rename(&tmp, dest)
            .with_context(|| format!("moving {} → {}", tmp.display(), dest.display()))?;
        Ok(true)
    }

    fn put(&self, key: &CacheKey, bundle_path: &Path) -> Result<()> {
        let data = std::fs::read(bundle_path)
            .with_context(|| format!("reading {}", bundle_path.display()))?;
        let path = self.object_path(key);
        let payload = PutPayload::from(Bytes::from(data));

        self.rt
            .block_on(self.store.put(&path, payload))
            .map_err(|e| anyhow::anyhow!("remote PUT to {path} failed: {e}"))?;
        Ok(())
    }

    fn display_url(&self) -> &str {
        &self.display_url
    }
}

/// Parse `s3://bucket/optional/prefix` into `(bucket, prefix)`.
/// Prefix is empty when the URL has no path beyond the bucket.
fn parse_s3_url(url: &str) -> Result<(String, String)> {
    let stripped = url
        .strip_prefix("s3://")
        .ok_or_else(|| anyhow::anyhow!("remote URL must start with s3:// (got: {url})"))?;
    let mut parts = stripped.splitn(2, '/');
    let bucket = parts.next().unwrap_or("").to_string();
    if bucket.is_empty() {
        anyhow::bail!("remote URL has no bucket: {url}");
    }
    let prefix = parts.next().unwrap_or("").trim_end_matches('/').to_string();
    Ok((bucket, prefix))
}

fn tmp_sibling(final_path: &Path) -> std::path::PathBuf {
    let mut s = final_path.as_os_str().to_os_string();
    s.push(".remote-tmp");
    std::path::PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_s3_url_bucket_only() {
        let (bucket, prefix) = parse_s3_url("s3://my-bucket").unwrap();
        assert_eq!(bucket, "my-bucket");
        assert_eq!(prefix, "");
    }

    #[test]
    fn parse_s3_url_bucket_with_prefix() {
        let (bucket, prefix) = parse_s3_url("s3://my-bucket/team/xyz").unwrap();
        assert_eq!(bucket, "my-bucket");
        assert_eq!(prefix, "team/xyz");
    }

    #[test]
    fn parse_s3_url_trailing_slash_stripped() {
        let (_, prefix) = parse_s3_url("s3://bucket/pfx/").unwrap();
        assert_eq!(prefix, "pfx");
    }

    #[test]
    fn parse_s3_url_rejects_non_s3_scheme() {
        assert!(parse_s3_url("https://bucket/pfx").is_err());
    }

    #[test]
    fn parse_s3_url_rejects_empty_bucket() {
        assert!(parse_s3_url("s3://").is_err());
    }

    #[test]
    fn object_path_with_no_prefix() {
        let key = CacheKey::from_hex("deadbeef");
        let remote = make_test_remote("", "");
        assert_eq!(remote.object_path(&key).to_string(), "deadbeef.tar");
    }

    #[test]
    fn object_path_with_prefix() {
        let key = CacheKey::from_hex("deadbeef");
        let remote = make_test_remote("team/xyz", "");
        assert_eq!(
            remote.object_path(&key).to_string(),
            "team/xyz/deadbeef.tar"
        );
    }

    /// Fake S3Remote for path-construction tests (doesn't hit the
    /// network).
    fn make_test_remote(prefix: &str, display: &str) -> S3Remote {
        S3Remote {
            store: Box::new(object_store::memory::InMemory::new()),
            prefix: prefix.to_string(),
            display_url: display.to_string(),
            rt: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        }
    }

    #[test]
    fn round_trip_via_in_memory_store() {
        let remote = make_test_remote("", "s3://test");
        let key = CacheKey::from_hex("aabb".repeat(16));
        let tmp = tempfile::tempdir().unwrap();

        let bundle = tmp.path().join("bundle.tar");
        std::fs::write(&bundle, b"fake tar data").unwrap();
        remote.put(&key, &bundle).unwrap();

        assert!(remote.has(&key));

        let dest = tmp.path().join("got.tar");
        assert!(remote.get(&key, &dest).unwrap());
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "fake tar data");
    }

    #[test]
    fn has_returns_false_for_missing_key() {
        let remote = make_test_remote("", "s3://test");
        let key = CacheKey::from_hex("00".repeat(32));
        assert!(!remote.has(&key));
    }

    #[test]
    fn get_returns_false_for_missing_key() {
        let remote = make_test_remote("", "s3://test");
        let key = CacheKey::from_hex("00".repeat(32));
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("got.tar");
        assert!(!remote.get(&key, &dest).unwrap());
        assert!(!dest.exists());
    }
}
