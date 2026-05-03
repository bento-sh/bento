//! Shared wire-protocol types for the bento content-addressable store.
//!
//! Same format whether the cache is self-hosted (S3-compatible) or the
//! hosted SaaS at `cache.bento.build`. Keeping these types in a standalone
//! crate means the bento CLI, the Cloudflare Worker that terminates the
//! hosted cache, and the control plane that mints tokens all share a single
//! source of truth.
//!
//! Nothing here does network I/O. This crate is pure types + validation +
//! (de)serialization; it compiles on `wasm32-unknown-unknown` alongside
//! native targets.
//!
//! # AX-first contract
//!
//! Enum discriminants serialize as stable `snake_case` strings — never
//! numeric — so agents can reason about responses without guessing which
//! integer meant what. New variants may be added (forward-compat) but
//! existing names are frozen. All public request/response types derive
//! [`schemars::JsonSchema`] so they can be exported into OpenAPI or MCP
//! schema files downstream.

#![deny(missing_docs)]

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt;

// ---------------------------------------------------------------------------
// Team + token identifiers
// ---------------------------------------------------------------------------

/// Opaque team identifier.
///
/// The control plane is the authority on the underlying value (UUID today,
/// could change). The CAS worker, CLI, and dashboard treat it as a bag of
/// bytes keyed by equality.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct TeamId(String);

impl TeamId {
    /// Wrap an existing string as a team id. No validation — the caller is
    /// trusting the control plane's format.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TeamId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------
// Token scope
// ---------------------------------------------------------------------------

/// Capability carried by a CLI token.
///
/// Serializes as a stable `snake_case` string (e.g. `"read_write"`). v2 will
/// add deploy-oriented variants (`DeployRead`, `DeployWrite`) without
/// breaking existing `read` / `read_write` / `admin` consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TokenScope {
    /// Read cache entries only. Safe default for CI read-through scenarios.
    Read,
    /// Read and write cache entries. Typical CI worker token.
    ReadWrite,
    /// Full control: token + team management in addition to cache access.
    Admin,
}

impl TokenScope {
    /// True if this scope can perform cache read operations.
    pub fn can_read(self) -> bool {
        matches!(self, Self::Read | Self::ReadWrite | Self::Admin)
    }

    /// True if this scope can perform cache write operations.
    pub fn can_write(self) -> bool {
        matches!(self, Self::ReadWrite | Self::Admin)
    }

    /// True if this scope can perform administrative operations (token
    /// rotation, team management).
    pub fn can_admin(self) -> bool {
        matches!(self, Self::Admin)
    }
}

impl fmt::Display for TokenScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Read => "read",
            Self::ReadWrite => "read_write",
            Self::Admin => "admin",
        };
        f.write_str(s)
    }
}

// ---------------------------------------------------------------------------
// Token label (user-supplied audit tag)
// ---------------------------------------------------------------------------

/// Human-readable label attached to a token at creation time, for audit
/// readability in the dashboard (e.g. `"ci-prod"`, `"dev-laptop"`).
///
/// Validated on construction: 3–40 chars of lowercase ASCII letters, digits,
/// or dashes. Leading/trailing dashes are rejected to keep CLI output and
/// URLs tidy.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, JsonSchema)]
#[serde(transparent)]
pub struct TokenLabel(String);

/// Minimum allowed [`TokenLabel`] length.
pub const TOKEN_LABEL_MIN_LEN: usize = 3;

/// Maximum allowed [`TokenLabel`] length.
pub const TOKEN_LABEL_MAX_LEN: usize = 40;

impl TokenLabel {
    /// Validate and wrap a string as a [`TokenLabel`].
    pub fn new(s: impl Into<String>) -> Result<Self, TokenLabelError> {
        let s = s.into();
        if s.len() < TOKEN_LABEL_MIN_LEN {
            return Err(TokenLabelError::TooShort {
                min: TOKEN_LABEL_MIN_LEN,
                got: s.len(),
            });
        }
        if s.len() > TOKEN_LABEL_MAX_LEN {
            return Err(TokenLabelError::TooLong {
                max: TOKEN_LABEL_MAX_LEN,
                got: s.len(),
            });
        }
        if s.starts_with('-') || s.ends_with('-') {
            return Err(TokenLabelError::LeadingOrTrailingDash);
        }
        if let Some(c) = s
            .chars()
            .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-'))
        {
            return Err(TokenLabelError::InvalidChar { found: c });
        }
        Ok(Self(s))
    }

    /// Borrow the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TokenLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for TokenLabel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

/// Why a [`TokenLabel`] failed validation.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TokenLabelError {
    /// Label shorter than [`TOKEN_LABEL_MIN_LEN`].
    #[error("token label too short: min {min} chars, got {got}")]
    TooShort {
        /// Minimum length required.
        min: usize,
        /// Length of the input.
        got: usize,
    },
    /// Label longer than [`TOKEN_LABEL_MAX_LEN`].
    #[error("token label too long: max {max} chars, got {got}")]
    TooLong {
        /// Maximum length allowed.
        max: usize,
        /// Length of the input.
        got: usize,
    },
    /// Label contained a character outside `[a-z0-9-]`.
    #[error("token label contains invalid character '{found}' (allowed: a-z, 0-9, -)")]
    InvalidChar {
        /// Offending character.
        found: char,
    },
    /// Label starts or ends with a dash.
    #[error("token label must not start or end with '-'")]
    LeadingOrTrailingDash,
}

// ---------------------------------------------------------------------------
// JWT claims
// ---------------------------------------------------------------------------

/// Payload carried by a bento cache JWT.
///
/// Keys are kept short to minimize token size (JWTs live in every CLI HTTP
/// request). `kid` (key identifier, used for rotation) lives in the JWT
/// *header*, not here — `jsonwebtoken`-style libraries handle it above this
/// layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Claims {
    /// Issuer. Always `"bento.build"` for the hosted service.
    pub iss: String,
    /// Team this token grants access to.
    pub team_id: TeamId,
    /// Capabilities attached to the token.
    pub scope: TokenScope,
    /// User-supplied audit label.
    pub label: TokenLabel,
    /// Issued-at time, seconds since the UNIX epoch.
    pub iat: u64,
    /// Expiry time, seconds since the UNIX epoch.
    pub exp: u64,
}

// ---------------------------------------------------------------------------
// Cache errors (typed status-code mapper)
// ---------------------------------------------------------------------------

/// Typed error kinds the CAS edge returns. Each variant maps to a single
/// HTTP status code via [`CacheError::http_status`].
///
/// Serializes as `{"kind": "<snake_case>"}`. Variants are additive only —
/// renaming or removing one is a breaking change.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CacheError {
    /// Key not present in the cache (404).
    #[error("not found")]
    NotFound,
    /// Authentication failed: missing or malformed token (401).
    #[error("unauthorized")]
    Unauthorized,
    /// Authenticated, but the token's scope does not cover this op (403).
    #[error("forbidden")]
    Forbidden,
    /// Team has exceeded its storage or request quota (413).
    #[error("over quota")]
    OverQuota,
    /// Malformed request — bad key, bad headers, etc. (400).
    #[error("bad request")]
    BadRequest,
    /// Edge or origin failure — server bug, R2 outage, etc. (500).
    #[error("internal error")]
    Internal,
}

impl CacheError {
    /// HTTP status code that should accompany this error in a response.
    pub fn http_status(&self) -> u16 {
        match self {
            Self::NotFound => 404,
            Self::Unauthorized => 401,
            Self::Forbidden => 403,
            Self::OverQuota => 413,
            Self::BadRequest => 400,
            Self::Internal => 500,
        }
    }
}

/// JSON body returned for error responses.
///
/// Keeps the typed [`CacheError`] alongside a free-form message. Agents
/// should switch on `error.kind`; humans read `message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ErrorResponse {
    /// Machine-readable error kind.
    pub error: CacheError,
    /// Human-readable detail. May be omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl ErrorResponse {
    /// Build a response for `error` with no accompanying message.
    pub fn new(error: CacheError) -> Self {
        Self {
            error,
            message: None,
        }
    }

    /// Build a response for `error` with a human-readable detail.
    pub fn with_message(error: CacheError, message: impl Into<String>) -> Self {
        Self {
            error,
            message: Some(message.into()),
        }
    }
}

// ---------------------------------------------------------------------------
// CAS response markers
// ---------------------------------------------------------------------------
//
// The CAS hot path (HEAD/GET/PUT) is pure HTTP — status codes plus raw
// bytes. The types below exist so handler signatures can be written in
// terms of `Result<CacheHeadResponse, CacheError>` rather than `Result<(),
// CacheError>`, giving us a place to grow metadata (e.g. size, cache
// origin) without breaking the handler surface.

/// Successful response for `HEAD /cache/<key>`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CacheHeadResponse;

/// Successful response for `GET /cache/<key>` (body streamed separately).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CacheGetResponse;

/// Successful response for `PUT /cache/<key>`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CachePutResponse;

// ---------------------------------------------------------------------------
// Large-upload presigned flow
// ---------------------------------------------------------------------------
//
// Bundles above the worker's body-size ceiling (Cloudflare's 100 MB edge
// limit) can't be PUT through the worker. The client calls
// `POST /cache/<key>/upload-url` to get a short-lived presigned R2 URL,
// uploads the bundle body directly to R2, then calls
// `POST /cache/<key>/complete` so the worker can verify the upload
// landed and fire the usual metering event. Small bundles stay on the
// existing `PUT /cache/<key>` path for the extra-round-trip savings.

/// Request body for `POST /cache/<key>/upload-url`. Size is advisory —
/// the worker uses it for logging / early quota rejection, not for
/// constraining the signed URL (R2 enforces nothing here).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UploadUrlRequest {
    /// Advertised bundle size in bytes. Used for early over-quota
    /// rejection so the client doesn't waste a full PUT on a doomed
    /// upload. Not authoritative — real byte count is measured at
    /// `complete` time via R2 HEAD.
    pub size_bytes: u64,
}

/// Successful response for `POST /cache/<key>/upload-url`.
///
/// The presigned URL is a fully-qualified `https://...` pointing at R2's
/// S3-compatible endpoint. The client must PUT the bundle body to that
/// URL with exactly the `headers` map on the request — any mismatch
/// invalidates the signature and R2 rejects with 403.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UploadUrlResponse {
    /// Presigned PUT URL. Short-lived (typically 5 minutes); resend the
    /// `upload-url` request on expiry.
    pub url: String,
    /// Unix-epoch seconds after which `url` is no longer valid.
    pub expires_at_unix_seconds: i64,
    /// Headers the client MUST send verbatim with the PUT, or R2's
    /// signature check rejects. Typically just
    /// `{"x-amz-content-sha256": "UNSIGNED-PAYLOAD"}` — the unsigned-
    /// payload mode keeps the signature independent of body content so
    /// we can sign without the client needing to hash its bundle first.
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
}

/// Successful response for `POST /cache/<key>/complete`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UploadCompleteResponse {
    /// Actual bytes landed in R2, measured via HEAD. Authoritative for
    /// metering. May differ from the client's `size_bytes` hint on
    /// `upload-url` if the client lied or the upload truncated.
    pub size_bytes: u64,
}

// ---------------------------------------------------------------------------
// Build report (cache-protocol extension for dashboards)
// ---------------------------------------------------------------------------
//
// Posted by the bento CLI to its configured remote cache server at the
// end of every `bento ci` / `bento build` invocation:
//
//   POST <bento-base>/report/build
//   Authorization: Bearer <JWT>     (same auth as cache writes)
//   Body: BuildReport JSON
//
// The cache server (cas-worker for the hosted product; user-defined for
// self-hosters) terminates the request and decides what to do with it —
// the bento CLI knows nothing about a "control plane" or "dashboard".
// Self-hosters can implement `/report/build` against any backend, or
// reject it with 404 and bento will silently no-op (best-effort).

/// Build summary emitted by the bento CLI at the end of a `bento ci`
/// or `bento build` invocation. One per top-level invocation, not per
/// dish — the CLI rolls per-dish results into a single report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct BuildReport {
    /// Bento or dish name the invocation targeted ("prod", "api/server").
    pub package: String,
    /// Git branch, if available. Best-effort: pulled from `$CI_BRANCH` /
    /// `git symbolic-ref` / similar. `None` for detached-HEAD shells or
    /// non-git workspaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Git SHA, if available. Server-side stays agnostic about format
    /// (full 40-char or short) — stored verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// Fraction of tasks served from cache, `0.0..=1.0`.
    pub cache_hit_ratio: f32,
    /// Outcome.
    pub status: BuildStatus,
    /// Wall-clock duration of the invocation in milliseconds.
    pub duration_ms: u64,
}

/// Outcome of a `bento ci` / `bento build` invocation.
///
/// Serializes as a stable `snake_case` string. Variants are additive
/// only — renaming or removing one is a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BuildStatus {
    /// Every planned task succeeded.
    Success,
    /// At least one planned task failed.
    Failed,
    /// Some tasks succeeded, others were skipped (preflight failure
    /// mid-flight, partial cancellation). Distinct from `Failed`
    /// because the invocation didn't crash — it just didn't complete
    /// the planned graph.
    Partial,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_label_accepts_valid() {
        for s in ["ci-prod", "dev-laptop", "v23", "a-b-c", "abc", "ci-prod-v2"] {
            TokenLabel::new(s).unwrap_or_else(|e| panic!("{s:?} should be valid: {e}"));
        }
    }

    #[test]
    fn token_label_rejects_too_short() {
        let err = TokenLabel::new("ab").unwrap_err();
        assert!(matches!(err, TokenLabelError::TooShort { .. }));
    }

    #[test]
    fn token_label_rejects_too_long() {
        let s = "a".repeat(TOKEN_LABEL_MAX_LEN + 1);
        let err = TokenLabel::new(s).unwrap_err();
        assert!(matches!(err, TokenLabelError::TooLong { .. }));
    }

    #[test]
    fn token_label_rejects_invalid_chars() {
        for s in ["CI-PROD", "ci_prod", "ci prod", "ci.prod", "ci/prod"] {
            let err = TokenLabel::new(s).unwrap_err();
            assert!(
                matches!(err, TokenLabelError::InvalidChar { .. }),
                "{s:?} should fail with InvalidChar, got {err:?}"
            );
        }
    }

    #[test]
    fn token_label_rejects_leading_or_trailing_dash() {
        for s in ["-ci", "ci-", "-ci-"] {
            let err = TokenLabel::new(s).unwrap_err();
            assert!(matches!(err, TokenLabelError::LeadingOrTrailingDash));
        }
    }

    #[test]
    fn token_label_deserialize_validates() {
        let ok: TokenLabel = serde_json::from_str("\"ci-prod\"").unwrap();
        assert_eq!(ok.as_str(), "ci-prod");
        let err = serde_json::from_str::<TokenLabel>("\"CI-PROD\"").unwrap_err();
        assert!(err.to_string().contains("invalid character"));
    }

    #[test]
    fn token_scope_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&TokenScope::Read).unwrap(),
            "\"read\""
        );
        assert_eq!(
            serde_json::to_string(&TokenScope::ReadWrite).unwrap(),
            "\"read_write\""
        );
        assert_eq!(
            serde_json::to_string(&TokenScope::Admin).unwrap(),
            "\"admin\""
        );
    }

    #[test]
    fn token_scope_capabilities() {
        assert!(TokenScope::Read.can_read() && !TokenScope::Read.can_write());
        assert!(TokenScope::ReadWrite.can_read() && TokenScope::ReadWrite.can_write());
        assert!(TokenScope::Admin.can_admin() && TokenScope::Admin.can_write());
        assert!(!TokenScope::ReadWrite.can_admin());
    }

    #[test]
    fn claims_roundtrip() {
        let claims = Claims {
            iss: "bento.build".to_owned(),
            team_id: TeamId::new("team_01HZ"),
            scope: TokenScope::ReadWrite,
            label: TokenLabel::new("ci-prod").unwrap(),
            iat: 1_700_000_000,
            exp: 1_700_000_000 + 31_536_000,
        };
        let json = serde_json::to_string(&claims).unwrap();
        let back: Claims = serde_json::from_str(&json).unwrap();
        assert_eq!(claims, back);
        // Stable wire field names — don't let anyone rename without thought.
        for field in ["iss", "team_id", "scope", "label", "iat", "exp"] {
            assert!(json.contains(field), "missing field {field} in {json}");
        }
        assert!(json.contains("\"read_write\""));
    }

    #[test]
    fn cache_error_status_codes() {
        assert_eq!(CacheError::NotFound.http_status(), 404);
        assert_eq!(CacheError::Unauthorized.http_status(), 401);
        assert_eq!(CacheError::Forbidden.http_status(), 403);
        assert_eq!(CacheError::OverQuota.http_status(), 413);
        assert_eq!(CacheError::BadRequest.http_status(), 400);
        assert_eq!(CacheError::Internal.http_status(), 500);
    }

    #[test]
    fn cache_error_tagged_snake_case() {
        let json = serde_json::to_string(&CacheError::OverQuota).unwrap();
        assert_eq!(json, "{\"kind\":\"over_quota\"}");
        let back: CacheError = serde_json::from_str(&json).unwrap();
        assert_eq!(back, CacheError::OverQuota);
    }

    #[test]
    fn error_response_omits_empty_message() {
        let json = serde_json::to_string(&ErrorResponse::new(CacheError::NotFound)).unwrap();
        assert!(!json.contains("message"));
        let json = serde_json::to_string(&ErrorResponse::with_message(
            CacheError::BadRequest,
            "bad key",
        ))
        .unwrap();
        assert!(json.contains("\"message\":\"bad key\""));
    }

    #[test]
    fn claims_exports_json_schema() {
        let schema = schemars::schema_for!(Claims);
        let json = serde_json::to_string(&schema).unwrap();
        for field in ["iss", "team_id", "scope", "label", "iat", "exp"] {
            assert!(json.contains(field), "schema missing field {field}");
        }
        // Scope must describe its valid string values (AX-readable enum).
        for variant in ["read", "read_write", "admin"] {
            assert!(
                json.contains(variant),
                "schema missing scope variant {variant}"
            );
        }
    }

    #[test]
    fn build_report_roundtrip() {
        let r = BuildReport {
            package: "api/server".to_owned(),
            branch: Some("main".to_owned()),
            sha: Some("abc1234".to_owned()),
            cache_hit_ratio: 0.93,
            status: BuildStatus::Success,
            duration_ms: 1_640,
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: BuildReport = serde_json::from_str(&json).unwrap();
        assert_eq!(r, back);
        for field in [
            "package",
            "branch",
            "sha",
            "cache_hit_ratio",
            "status",
            "duration_ms",
        ] {
            assert!(json.contains(field), "missing field {field} in {json}");
        }
        assert!(json.contains("\"success\""));
    }

    #[test]
    fn build_report_omits_empty_optional_fields() {
        let r = BuildReport {
            package: "marketing".to_owned(),
            branch: None,
            sha: None,
            cache_hit_ratio: 0.0,
            status: BuildStatus::Failed,
            duration_ms: 412,
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("branch"));
        assert!(!json.contains("sha"));
        assert!(json.contains("\"failed\""));
    }

    #[test]
    fn build_status_serializes_snake_case() {
        for (variant, expected) in [
            (BuildStatus::Success, "\"success\""),
            (BuildStatus::Failed, "\"failed\""),
            (BuildStatus::Partial, "\"partial\""),
        ] {
            assert_eq!(serde_json::to_string(&variant).unwrap(), expected);
        }
    }

    #[test]
    fn cache_error_exports_json_schema() {
        let schema = schemars::schema_for!(CacheError);
        let json = serde_json::to_string(&schema).unwrap();
        for variant in [
            "not_found",
            "unauthorized",
            "forbidden",
            "over_quota",
            "bad_request",
            "internal",
        ] {
            assert!(json.contains(variant), "schema missing variant {variant}");
        }
    }
}
