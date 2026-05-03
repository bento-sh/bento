//! Bearer-auth remote cache backend — the `bento://` URL scheme.
//!
//! Config:
//!
//! ```toml
//! [cache]
//! remote           = "bento://cache.bento.build"
//! remote_token_env = "BENTO_CACHE_TOKEN"   # env var holding the JWT
//! ```
//!
//! Wire protocol:
//!
//! - `HEAD <base>/cache/<blake3>` — 200 hit, 404 miss, 401/403 auth.
//! - `GET  <base>/cache/<blake3>` — 200 + body, 404 miss, 413 over-quota.
//! - `PUT  <base>/cache/<blake3>` — 2xx stored, 413 over-quota, 4xx auth.
//!
//! The hosted bento.build cache lives at `cache.bento.build` and derives
//! the team id from the JWT. Any server implementing the same shape can
//! be pointed at via `bento://<host>[/<prefix>]`.
//!
//! # Failure semantics
//!
//! Same contract as [`crate::remote::S3Remote`]:
//!
//! - `has` → `false` on any non-200 / error (best-effort).
//! - `get` → `Ok(false)` on miss / transport / body-read failure;
//!   only `Err` for local write problems.
//! - `put` → `Err` propagates; executor logs + keeps the local bundle.
//!
//! # Retries
//!
//! One retry on 5xx or connection errors, 500 ms back-off. We deliberately
//! don't retry further — the executor already has a per-task retry ladder,
//! and the hot path cares about latency more than cache-hit optimality.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::key::CacheKey;
use crate::remote_api::RemoteCache;

/// HTTP-based remote cache using Bearer-auth'd JWTs.
pub struct BearerRemote {
    client: reqwest::Client,
    /// Pre-resolved HTTPS base, no trailing slash (e.g.
    /// `https://cache.bento.build`). Endpoint paths append
    /// `/cache/<hex>` to this.
    http_base: String,
    /// Original `bento://…` URL, kept for logs.
    display_url: String,
    /// JWT presented as `Authorization: Bearer <token>`.
    token: String,
    /// Owned single-threaded tokio runtime; each op `block_on`s here so
    /// the executor's thread stays synchronous (same shape as S3Remote).
    rt: tokio::runtime::Runtime,
}

/// Connect timeout per HTTP attempt. Hosted cache is expected to be
/// globally-replicated; more than a few seconds of RTT means something's
/// wrong, and we'd rather fall through to local execution than stall.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Total timeout per HTTP attempt. Big enough for a cold bundle
/// download on a slow network; small enough that a hung proxy gets
/// noticed.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Back-off between the first attempt and the single retry.
const RETRY_BACKOFF: Duration = Duration::from_millis(500);

/// Split threshold for `bento://` uploads. Bundles at or above this
/// size skip the worker-proxied `PUT /cache/<key>` path (which would hit
/// Cloudflare's 100 MB edge body cap) and use the presigned-R2-URL
/// flow instead (`POST /cache/<key>/upload-url` → direct PUT to R2 →
/// `POST /cache/<key>/complete`). 95 MB leaves headroom for HTTP
/// framing under the 100 MB edge ceiling.
const BENTO_PRESIGN_THRESHOLD_BYTES: usize = 95 * 1024 * 1024;

/// Heuristic: is this 413 response body from Cloudflare's edge (before
/// the worker runs), rather than from the worker's own over-quota check?
/// Edge rejections are HTML-branded; worker rejections are JSON with
/// `error: "over_quota"`.
fn looks_like_edge_413(body_snippet: &str) -> bool {
    let lower = body_snippet.to_lowercase();
    lower.contains("cloudflare") || lower.contains("<html") || lower.contains("payload too large")
}

impl BearerRemote {
    /// Build a bearer-auth remote for `bento_url` (e.g.
    /// `bento://cache.bento.build`) using `token` as the JWT.
    ///
    /// Fails on malformed URL or client construction problems; callers
    /// typically log + disable the remote tier on `Err`.
    pub fn new(bento_url: &str, token: String) -> Result<Self> {
        let http_base = parse_bento_url(bento_url)?;
        let display_url = bento_url.to_string();
        Self::with_http_base(http_base, display_url, token)
    }

    /// Build a remote pointing directly at an `http://` / `https://`
    /// base URL. Used by integration tests against a local mock server;
    /// production callers go through [`BearerRemote::new`] with a
    /// `bento://` URL.
    #[doc(hidden)]
    pub fn from_http_base(http_base: impl Into<String>, token: String) -> Result<Self> {
        let http_base = http_base.into();
        let display = http_base.clone();
        Self::with_http_base(http_base, display, token)
    }

    fn with_http_base(http_base: String, display_url: String, token: String) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("bento/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building reqwest client for bento:// remote cache")?;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("creating tokio runtime for bento:// remote cache")?;

        Ok(Self {
            client,
            http_base,
            display_url,
            token,
            rt,
        })
    }

    fn cache_url(&self, key: &CacheKey) -> String {
        format!("{}/cache/{}", self.http_base, key.as_hex())
    }

    fn upload_url_endpoint(&self, key: &CacheKey) -> String {
        format!("{}/cache/{}/upload-url", self.http_base, key.as_hex())
    }

    fn complete_endpoint(&self, key: &CacheKey) -> String {
        format!("{}/cache/{}/complete", self.http_base, key.as_hex())
    }

    /// Presigned-URL upload path for bundles above the edge body-size
    /// ceiling. Three calls: ask the worker for a presigned R2 URL,
    /// PUT the bundle there directly (bypassing the worker), then tell
    /// the worker to HEAD R2 + fire metering.
    fn put_via_presigned_url(
        &self,
        key: &CacheKey,
        bundle_path: &Path,
        data: Vec<u8>,
    ) -> Result<()> {
        let size_bytes = data.len() as u64;
        let upload_url_endpoint = self.upload_url_endpoint(key);
        let complete_endpoint = self.complete_endpoint(key);
        let token = &self.token;
        let client = &self.client;

        // 1. Ask the worker for a presigned PUT URL.
        let presigned: bento_cas_protocol::UploadUrlResponse = self.rt.block_on(async {
            let body = bento_cas_protocol::UploadUrlRequest { size_bytes };
            let resp = self
                .send_with_retry(|| {
                    client
                        .post(&upload_url_endpoint)
                        .bearer_auth(token)
                        .json(&body)
                })
                .await
                .map_err(|e| anyhow::anyhow!("upload-url POST {upload_url_endpoint}: {e}"))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("upload-url POST {upload_url_endpoint}: {status} — {text}");
            }
            resp.json::<bento_cas_protocol::UploadUrlResponse>()
                .await
                .map_err(|e| anyhow::anyhow!("parsing upload-url response: {e}"))
        })?;

        // 2. PUT the bundle body to the presigned URL. No bearer auth
        // — the presigned URL carries its own SigV4 signature. Headers
        // from the response must be sent verbatim or R2 rejects.
        let body = bytes::Bytes::from(data);
        let status = self.rt.block_on(async {
            let mut req = client
                .put(&presigned.url)
                .header(reqwest::header::CONTENT_LENGTH, body.len())
                .body(body.clone());
            for (k, v) in &presigned.headers {
                req = req.header(k, v);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("R2 PUT {}: {e}", presigned.url))?;
            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                let snippet: String = text.chars().take(200).collect();
                anyhow::bail!(
                    "R2 PUT ({} bytes) from {} failed: {status} — {snippet}",
                    size_bytes,
                    bundle_path.display()
                );
            }
            Ok::<_, anyhow::Error>(status)
        })?;
        tracing::debug!("R2 PUT succeeded: {} ({} bytes)", status, size_bytes);

        // 3. Tell the worker we're done — it HEADs R2 to confirm + fires
        // the metering event. A 404 here means our PUT didn't actually
        // land despite the 2xx above, which would be a bug we want to
        // surface.
        self.rt.block_on(async {
            let resp = self
                .send_with_retry(|| client.post(&complete_endpoint).bearer_auth(token))
                .await
                .map_err(|e| anyhow::anyhow!("complete POST {complete_endpoint}: {e}"))?;
            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("complete POST {complete_endpoint}: {status} — {text}");
            }
            Ok::<_, anyhow::Error>(())
        })?;

        Ok(())
    }

    async fn send_with_retry(
        &self,
        builder: impl Fn() -> reqwest::RequestBuilder,
    ) -> reqwest::Result<reqwest::Response> {
        match builder().send().await {
            Ok(r) if should_retry_status(r.status()) => {
                tracing::debug!(
                    "remote cache returned {}, retrying once after {:?}",
                    r.status(),
                    RETRY_BACKOFF
                );
                tokio::time::sleep(RETRY_BACKOFF).await;
                builder().send().await
            }
            Ok(r) => Ok(r),
            Err(e) if e.is_connect() || e.is_timeout() => {
                tracing::debug!("remote cache connection error, retrying once: {e}");
                tokio::time::sleep(RETRY_BACKOFF).await;
                builder().send().await
            }
            Err(e) => Err(e),
        }
    }
}

fn should_retry_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

impl RemoteCache for BearerRemote {
    fn has(&self, key: &CacheKey) -> bool {
        let url = self.cache_url(key);
        let token = &self.token;
        let client = &self.client;

        let result = self
            .rt
            .block_on(self.send_with_retry(|| client.head(&url).bearer_auth(token)));

        match result {
            Ok(r) if r.status().is_success() => true,
            Ok(r) if r.status() == reqwest::StatusCode::NOT_FOUND => false,
            Ok(r) => {
                tracing::debug!("remote HEAD {url} → {}", r.status());
                false
            }
            Err(e) => {
                tracing::debug!("remote HEAD {url} failed: {e}");
                false
            }
        }
    }

    fn get(&self, key: &CacheKey, dest: &Path) -> Result<bool> {
        let url = self.cache_url(key);
        let token = &self.token;
        let client = &self.client;

        let bytes: Option<bytes::Bytes> = self.rt.block_on(async {
            let resp = match self
                .send_with_retry(|| client.get(&url).bearer_auth(token))
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("remote GET {url} failed: {e}");
                    return None;
                }
            };
            match resp.status().as_u16() {
                200 => match resp.bytes().await {
                    Ok(b) => Some(b),
                    Err(e) => {
                        tracing::warn!("remote GET {url} body read failed: {e}");
                        None
                    }
                },
                404 => None,
                other => {
                    tracing::warn!("remote GET {url} → {other}");
                    None
                }
            }
        });

        let Some(data) = bytes else {
            return Ok(false);
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

        // Cloudflare's edge caps request bodies at 100 MB on Free +
        // Standard paid plans — before the worker runs, before R2 sees
        // the body. For bundles above the split threshold we route
        // through the worker's presigned-URL flow so the client uploads
        // directly to R2 and bypasses the worker body-size limit.
        if data.len() >= BENTO_PRESIGN_THRESHOLD_BYTES {
            return self.put_via_presigned_url(key, bundle_path, data);
        }

        let url = self.cache_url(key);
        let token = &self.token;
        let client = &self.client;
        let body = bytes::Bytes::from(data);

        let resp = self
            .rt
            .block_on(self.send_with_retry(|| {
                client
                    .put(&url)
                    .bearer_auth(token)
                    .header(reqwest::header::CONTENT_TYPE, "application/x-tar")
                    .body(body.clone())
            }))
            .map_err(|e| anyhow::anyhow!("remote PUT {url}: {e}"))?;

        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        if status == reqwest::StatusCode::PAYLOAD_TOO_LARGE {
            // Read a snippet of the body to distinguish Cloudflare's
            // edge-origin 413 (HTML, "cloudflare" brand) from the
            // worker's own over-quota 413 (JSON `error: over_quota`).
            // Misreporting one as the other has real debugging cost.
            let body = self
                .rt
                .block_on(async { resp.text().await.unwrap_or_default() });
            let snippet = body.chars().take(200).collect::<String>();
            if looks_like_edge_413(&snippet) {
                anyhow::bail!(
                    "remote PUT {url}: 413 from Cloudflare's edge — body exceeds the \
                     platform's request-size limit (not a quota issue)."
                );
            }
            anyhow::bail!("remote PUT {url}: 413 over quota");
        }
        anyhow::bail!("remote PUT {url}: {status}")
    }

    fn display_url(&self) -> &str {
        &self.display_url
    }
}

/// Parse `bento://host[/extra/path]` into an HTTPS base.
///
/// `bento://cache.bento.build` → `https://cache.bento.build`.
/// `bento://host/prefix` → `https://host/prefix` (caller then appends
/// `/cache/<hex>`).
fn parse_bento_url(url: &str) -> Result<String> {
    let rest = url
        .strip_prefix("bento://")
        .ok_or_else(|| anyhow::anyhow!("bento URL must start with bento:// (got: {url})"))?;
    if rest.is_empty() {
        anyhow::bail!("bento URL has no host: {url}");
    }
    let trimmed = rest.trim_end_matches('/');
    if trimmed.is_empty() {
        anyhow::bail!("bento URL has no host: {url}");
    }
    let mut parts = trimmed.splitn(2, '/');
    let host = parts.next().unwrap_or("");
    if host.is_empty() {
        anyhow::bail!("bento URL has no host: {url}");
    }
    Ok(match parts.next() {
        Some(path) if !path.is_empty() => format!("https://{host}/{path}"),
        _ => format!("https://{host}"),
    })
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
    fn parse_bento_url_host_only() {
        assert_eq!(
            parse_bento_url("bento://cache.bento.build").unwrap(),
            "https://cache.bento.build"
        );
    }

    #[test]
    fn parse_bento_url_with_prefix() {
        assert_eq!(
            parse_bento_url("bento://cache.example.com/team/xyz").unwrap(),
            "https://cache.example.com/team/xyz"
        );
    }

    #[test]
    fn parse_bento_url_trailing_slash_stripped() {
        assert_eq!(
            parse_bento_url("bento://cache.bento.build/").unwrap(),
            "https://cache.bento.build"
        );
    }

    #[test]
    fn parse_bento_url_rejects_wrong_scheme() {
        assert!(parse_bento_url("https://cache.bento.build").is_err());
        assert!(parse_bento_url("s3://bucket").is_err());
    }

    #[test]
    fn parse_bento_url_rejects_empty_host() {
        assert!(parse_bento_url("bento://").is_err());
        assert!(parse_bento_url("bento:///no-host").is_err());
    }

    #[test]
    fn detects_cloudflare_edge_413_page() {
        // Real Cloudflare edge response body.
        let edge = "<html>\n<head><title>413 Payload Too Large</title></head>\n\
                    <body>\n<center><h1>413 Payload Too Large</h1></center>\n\
                    <hr><center>cloudflare</center>\n</body>\n</html>";
        assert!(looks_like_edge_413(edge));
    }

    #[test]
    fn does_not_misidentify_worker_413_as_edge() {
        // Worker's over-quota response: JSON body.
        let worker = r#"{"error":"over_quota","message":"free tier exceeded"}"#;
        assert!(!looks_like_edge_413(worker));
    }

    #[test]
    fn cache_url_format() {
        let remote = test_remote("https://cache.example.com");
        let key = CacheKey::from_hex("deadbeef");
        assert_eq!(
            remote.cache_url(&key),
            "https://cache.example.com/cache/deadbeef"
        );
    }

    #[test]
    fn cache_url_with_prefix() {
        let remote = test_remote("https://cache.example.com/team/xyz");
        let key = CacheKey::from_hex("deadbeef");
        assert_eq!(
            remote.cache_url(&key),
            "https://cache.example.com/team/xyz/cache/deadbeef"
        );
    }

    #[test]
    fn display_url_is_original() {
        let r = BearerRemote::new("bento://cache.example.com", "tok".into()).unwrap();
        assert_eq!(r.display_url(), "bento://cache.example.com");
    }

    #[test]
    fn should_retry_status_classifies() {
        assert!(should_retry_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
        assert!(should_retry_status(reqwest::StatusCode::BAD_GATEWAY));
        assert!(should_retry_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(!should_retry_status(reqwest::StatusCode::NOT_FOUND));
        assert!(!should_retry_status(reqwest::StatusCode::UNAUTHORIZED));
        assert!(!should_retry_status(reqwest::StatusCode::OK));
    }

    /// Construct a BearerRemote with the given http_base directly,
    /// bypassing URL parsing — used by cache_url_* tests.
    fn test_remote(http_base: &str) -> BearerRemote {
        BearerRemote {
            client: reqwest::Client::builder().build().unwrap(),
            http_base: http_base.to_string(),
            display_url: format!("bento://{}", http_base.trim_start_matches("https://")),
            token: "test-token".to_string(),
            rt: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        }
    }
}
