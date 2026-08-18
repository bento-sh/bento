//! Hosted control-plane client — the small read-only half of
//! `api.bento.build` the CLI needs.
//!
//! Two callers: `bento prime` folds [`CloudHealth`] into its snapshot
//! (best-effort, never fatal) and `bento cloud health` prints it. Both
//! authenticate with the cache JWT that `bento login` already stashed,
//! so there's nothing extra to configure.
//!
//! The team is addressed by the id inside the token — the only team
//! identifier a JWT carries — which is why nothing here asks for a
//! slug.

use std::io::Read;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Default control-plane base. Overridable via `BENTO_API_BASE` for
/// local dev against a preview deploy or a self-hosted control plane.
const DEFAULT_API_BASE: &str = "https://api.bento.build";
const API_BASE_ENV: &str = "BENTO_API_BASE";

/// Control-plane base URL, trailing slash stripped.
pub fn api_base() -> String {
    std::env::var(API_BASE_ENV)
        .ok()
        .map(|s| s.trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
}

/// `GET /v1/teams/:id/health` — cache + build health for the team the
/// token belongs to.
///
/// Field names mirror the control plane's JSON exactly. `flaky` and
/// `cold` are per *package* (one `bento ci` invocation = one row up
/// there), not per task.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CloudHealth {
    /// Team slug the token resolves to.
    pub team: String,
    /// Billing tier of the owning org.
    pub tier: String,
    pub cache: CloudCache,
    pub builds_7d: CloudBuilds,
    #[serde(default)]
    pub flaky_packages: Vec<FlakyPackage>,
    #[serde(default)]
    pub cold_packages: Vec<ColdPackage>,
    /// Server-side advice. `bento prime` appends these after its own.
    #[serde(default)]
    pub recommended_next: Vec<String>,
    /// RFC 3339 timestamp the control plane computed this at.
    pub generated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CloudCache {
    pub hit_rate_7d: f32,
    pub bytes_used: i64,
    pub bytes_limit: i64,
    pub pct_used: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CloudBuilds {
    pub total: i64,
    pub failed: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FlakyPackage {
    pub package: String,
    pub fail_rate_7d: f32,
    pub runs_7d: i64,
    pub first_seen: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ColdPackage {
    pub package: String,
    pub miss_rate_7d: f32,
    pub runs_7d: i64,
}

/// Fetch team health using `token`.
///
/// `timeout` is the whole-request budget — prime passes a short one
/// because it degrades to "no cloud section" rather than waiting.
pub fn fetch_health(token: &str, timeout: Duration) -> Result<CloudHealth> {
    let claims = decode_jwt_claims(token).map_err(|e| anyhow!("cache token: {e}"))?;
    let url = format!("{}/v1/teams/{}/health", api_base(), claims.team_id.as_str());
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    let resp = agent
        .get(&url)
        .set("authorization", &format!("Bearer {token}"))
        .call()
        .with_context(|| format!("GET {url}"))?;
    // Cap the read so a pathological server can't exhaust memory.
    let mut body = String::new();
    resp.into_reader()
        .take(256 * 1024)
        .read_to_string(&mut body)
        .context("reading health response")?;
    serde_json::from_str(&body).with_context(|| format!("parsing health response: {body}"))
}

/// Decode a JWT's payload segment into [`bento_cas_protocol::Claims`].
/// Does NOT verify the signature — the CLI doesn't hold the server's
/// public key. The point is to read the team id and catch shape /
/// claim errors locally so users don't waste a round-trip diagnosing
/// "why does every cache call return 401".
pub fn decode_jwt_claims(jwt: &str) -> Result<bento_cas_protocol::Claims, String> {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    let mut parts = jwt.split('.');
    let _header = parts.next().ok_or("token has no segments".to_string())?;
    let payload_b64 = parts
        .next()
        .ok_or("token missing payload segment".to_string())?;
    let _sig = parts
        .next()
        .ok_or("token missing signature segment".to_string())?;
    if parts.next().is_some() {
        return Err("token has more than 3 segments".to_string());
    }
    let payload = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| format!("payload is not valid base64url: {e}"))?;
    serde_json::from_slice::<bento_cas_protocol::Claims>(&payload)
        .map_err(|e| format!("payload JSON does not match Claims: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_decode_rejects_malformed_tokens() {
        assert!(decode_jwt_claims("").is_err());
        assert!(decode_jwt_claims("only-one-segment").is_err());
        assert!(decode_jwt_claims("a.b").is_err());
        assert!(decode_jwt_claims("a.b.c.d").is_err());
        // Valid header.payload.sig structure but payload isn't valid
        // base64url, then payload isn't valid Claims JSON.
        assert!(decode_jwt_claims("header.!!notb64!!.sig").is_err());
        assert!(decode_jwt_claims("header.eyJmb28iOiJiYXIifQ.sig").is_err());
    }

    #[test]
    fn jwt_decode_extracts_claims_and_team_id() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        let claims_json = serde_json::json!({
            "iss": "bento.build",
            "team_id": "00000000-0000-0000-0000-000000000001",
            "scope": "read_write",
            "label": "ci-prod",
            "iat": 1_700_000_000_u64,
            "exp": 1_700_000_000_u64 + 30 * 86_400,
        });
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims_json).unwrap());
        let jwt = format!("HEADER.{payload}.SIG");
        let claims = decode_jwt_claims(&jwt).expect("valid claims should decode");
        assert_eq!(claims.iss, "bento.build");
        assert_eq!(claims.label.as_str(), "ci-prod");
        // The team id is what `fetch_health` addresses the CP with.
        assert_eq!(
            claims.team_id.as_str(),
            "00000000-0000-0000-0000-000000000001"
        );
    }
}
