//! `bento login` — CLI half of the device-code flow.
//!
//! 1. POST `<api>/v1/cli/device-code` → `{device_code, user_code,
//!    verification_url, interval, expires_in}`.
//! 2. Print the verification URL + user_code; wait for the user to
//!    approve in the browser.
//! 3. Poll `<api>/v1/cli/exchange { device_code }` every `interval`
//!    seconds. The response is a tagged union on `status`:
//!    `pending` → keep polling; `approved` → JWT delivered;
//!    `expired` → bail, user re-runs.
//! 4. On 429 from the poll, double the interval (RFC device-code
//!    `slow_down`).
//! 5. Stash the JWT via [`bento_cache::token::store_cache_token`]
//!    (keychain → 0600 file fallback). Print where it landed, and what
//!    was actually granted (read from the token's own claims, not from
//!    what we asked for — the server has the last word).
//!
//! Scope + TTL travel in the device-code POST, so the approval page can
//! show them before anyone clicks Approve. Omitting both keeps the
//! historic default (read_write, 1 year); `--agent` asks for read / 1h.
//!
//! Output is intentionally terse: agents will run `bento login` in a
//! non-interactive harness and the fewer lines we emit, the easier
//! the verification URL is to grep out of stdout. Verbose progress
//! goes through `tracing::info!` when `-v` is passed.

use std::io::Read;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use bento_cache::token::{store_cache_token, TokenSink};

/// Classified login failure modes. Downcast through
/// [`crate::errors::classify`] so each variant becomes a distinct
/// `kind` in the structured envelope; agents can branch on
/// `login_expired` / `login_timeout` / `login_server_error` rather
/// than string-matching the anyhow message.
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error("device code expired or was revoked — re-run `bento login`")]
    Expired,

    #[error("timed out waiting for approval ({timeout_secs}s) — re-run `bento login`")]
    Timeout { timeout_secs: u64 },

    #[error("{stage} returned HTTP {status}: {body}")]
    ServerError {
        stage: &'static str,
        status: u16,
        body: String,
    },

    #[error("{stage} transport error: {source}")]
    Transport {
        stage: &'static str,
        #[source]
        source: anyhow::Error,
    },

    #[error("{stage}: malformed response body: {detail}")]
    InvalidResponse { stage: &'static str, detail: String },
}

/// Default API base for the bento.build hosted cache. Overridable via
/// `BENTO_API_BASE` (useful for local dev against a preview deploy or
/// a self-hosted control plane).
const DEFAULT_API_BASE: &str = "https://api.bento.build";
const API_BASE_ENV: &str = "BENTO_API_BASE";

/// Hard upper bound on the poll loop, belt-and-braces above the
/// server's own `expires_in`. Prevents a buggy server response of
/// `expires_in: 0` from turning the CLI into an infinite pending loop.
const MAX_WAIT: Duration = Duration::from_secs(900);

/// Cap on per-attempt HTTP timeout. Device-code + exchange are small
/// JSON; ten seconds is generous.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_url: String,
    interval: u32,
    expires_in: i64,
}

#[derive(Debug, Default, Serialize)]
struct DeviceCodeRequest {
    /// Omitted entirely when unset so the server applies its default
    /// rather than having to treat `null` as "unset".
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl_minutes: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ExchangeRequest<'a> {
    device_code: &'a str,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum ExchangeResponse {
    Pending,
    Approved { jwt: String },
    Expired,
}

/// `--scope` wins; then `--push`; then `--agent`; then the server's
/// default. `--ttl` likewise, with `--agent` implying 60 minutes.
pub fn run(agent: bool, push: bool, scope: Option<String>, ttl: Option<u32>) -> Result<i32> {
    let req = DeviceCodeRequest {
        scope: scope
            .or_else(|| push.then(|| "read_write".to_string()))
            .or_else(|| agent.then(|| "read".to_string())),
        ttl_minutes: ttl.or_else(|| agent.then_some(AGENT_TTL_MINUTES)),
    };
    let api_base = api_base();
    let device = request_device_code(&api_base, &req)?;

    println!(
        "To authorize this CLI, open:\n  {}",
        device.verification_url
    );
    println!("Device code: {}", device.user_code);
    println!(
        "Waiting for approval ({} min)…",
        device.expires_in.max(0) / 60
    );

    let jwt = poll_for_jwt(&api_base, &device)?;

    let sink = store_cache_token(&jwt).context("storing JWT")?;
    match sink {
        TokenSink::Keychain => println!("Logged in. Token stored in OS keychain."),
        TokenSink::File(path) => println!("Logged in. Token stored at {} (0600).", path.display()),
    }
    // Report what the server actually minted, not what we asked for.
    if let Ok(claims) = bento_core::cloud::decode_jwt_claims(&jwt) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        println!(
            "Granted: {} · expires in {} · label {}",
            claims.scope,
            format_minutes(claims.exp.saturating_sub(now) / 60),
            claims.label.as_str(),
        );
    }
    Ok(0)
}

/// Both login requests carry the same identity: a versioned UA and the
/// coarse client kind, so the control plane can tell an agent-driven
/// `bento login` from a human one at the device-code step.
fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(HTTP_TIMEOUT)
        .user_agent(&bento_cache::client_id::user_agent())
        .build()
}

/// `--agent`'s TTL. Long enough for a working session, short enough
/// that a leaked agent token is a non-event.
const AGENT_TTL_MINUTES: u32 = 60;

/// Parse `45m` / `2h` / `7d` into minutes. Bare numbers are rejected:
/// `--ttl 7` is ambiguous enough to be worth a error message.
///
/// Used as a clap `value_parser`, so the error text lands in `--help`
/// style output before any network call happens.
pub fn parse_ttl_minutes(raw: &str) -> Result<u32, String> {
    let s = raw.trim();
    let (digits, unit) = s.split_at(s.len().saturating_sub(1));
    let n: u32 = digits
        .parse()
        .map_err(|_| format!("bad --ttl {raw:?}: expected a number followed by m, h, or d"))?;
    let minutes = match unit {
        "m" => n,
        "h" => n.saturating_mul(60),
        "d" => n.saturating_mul(60 * 24),
        _ => return Err(format!("bad --ttl {raw:?}: unit must be m, h, or d")),
    };
    if minutes == 0 {
        return Err(format!("bad --ttl {raw:?}: must be at least 1 minute"));
    }
    if minutes > MAX_TTL_MINUTES {
        return Err(format!("bad --ttl {raw:?}: maximum is 365d"));
    }
    Ok(minutes)
}

/// Server-side cap, mirrored here so `--ttl 400d` fails locally with a
/// clear message instead of being silently clamped after a round trip.
const MAX_TTL_MINUTES: u32 = 365 * 24 * 60;

/// Coarse human duration for the granted-token line: minutes under an
/// hour, hours under a day, days above that.
fn format_minutes(minutes: u64) -> String {
    match minutes {
        0 => "under a minute".to_string(),
        m if m < 60 => format!("{m}m"),
        m if m < 60 * 24 => format!("{}h", m / 60),
        m => format!("{}d", m / (60 * 24)),
    }
}

fn api_base() -> String {
    std::env::var(API_BASE_ENV)
        .map(|s| s.trim_end_matches('/').to_string())
        .unwrap_or_else(|_| DEFAULT_API_BASE.to_string())
}

fn request_device_code(api_base: &str, req: &DeviceCodeRequest) -> Result<DeviceCodeResponse> {
    let url = format!("{api_base}/v1/cli/device-code");
    let agent = agent();
    let resp = agent
        .post(&url)
        .set("content-type", "application/json")
        .set(
            bento_cas_protocol::CLIENT_HEADER,
            bento_cache::client_id::detect(),
        )
        // `{}` when nothing was narrowed — the endpoint takes its
        // defaults from there and reads the client IP from headers.
        .send_string(&serde_json::to_string(req)?)
        .map_err(|e| classify_ureq("device-code", e))?;
    parse_json("device-code", resp)
}

fn poll_for_jwt(api_base: &str, device: &DeviceCodeResponse) -> Result<String> {
    let url = format!("{api_base}/v1/cli/exchange");
    let agent = agent();
    let body = serde_json::to_string(&ExchangeRequest {
        device_code: &device.device_code,
    })?;

    let mut interval = Duration::from_secs(device.interval.max(1) as u64);
    let start = Instant::now();
    let server_deadline = Duration::from_secs(device.expires_in.max(0) as u64);
    let hard_deadline = server_deadline.min(MAX_WAIT);

    loop {
        if start.elapsed() > hard_deadline {
            return Err(LoginError::Timeout {
                timeout_secs: hard_deadline.as_secs(),
            }
            .into());
        }
        std::thread::sleep(interval);

        match agent
            .post(&url)
            .set("content-type", "application/json")
            .set(
                bento_cas_protocol::CLIENT_HEADER,
                bento_cache::client_id::detect(),
            )
            .send_string(&body)
        {
            Ok(resp) => {
                let parsed: ExchangeResponse = parse_json("exchange", resp)?;
                match parsed {
                    ExchangeResponse::Pending => continue,
                    ExchangeResponse::Approved { jwt } => return Ok(jwt),
                    ExchangeResponse::Expired => {
                        return Err(LoginError::Expired.into());
                    }
                }
            }
            // 429 → slow down, double the interval (capped at 60s so
            // we don't silently walk off into a 10-minute sleep that
            // blows the expiry deadline).
            Err(ureq::Error::Status(429, _)) => {
                interval = (interval * 2).min(Duration::from_secs(60));
                tracing::debug!("exchange rate-limited; backing off to {:?}", interval);
                continue;
            }
            Err(ureq::Error::Status(status, r)) => {
                return Err(LoginError::ServerError {
                    stage: "exchange",
                    status,
                    body: response_body_snippet(r),
                }
                .into());
            }
            Err(ureq::Error::Transport(e)) => {
                // Transient — try again after the configured interval.
                tracing::debug!("exchange transport error: {e}");
                continue;
            }
        }
    }
}

fn parse_json<T: serde::de::DeserializeOwned>(
    stage: &'static str,
    resp: ureq::Response,
) -> Result<T> {
    // Cap the read so a pathological server can't exhaust memory.
    // Device-code + exchange responses are tiny (<1 KB); 64 KB is
    // three orders of magnitude of headroom.
    let mut buf = String::new();
    resp.into_reader()
        .take(64 * 1024)
        .read_to_string(&mut buf)
        .map_err(|e| LoginError::InvalidResponse {
            stage,
            detail: format!("reading response body: {e}"),
        })?;
    serde_json::from_str(&buf).map_err(|e| {
        LoginError::InvalidResponse {
            stage,
            detail: format!("malformed JSON ({e}): {buf}"),
        }
        .into()
    })
}

fn classify_ureq(stage: &'static str, err: ureq::Error) -> anyhow::Error {
    match err {
        ureq::Error::Status(status, r) => LoginError::ServerError {
            stage,
            status,
            body: response_body_snippet(r),
        }
        .into(),
        ureq::Error::Transport(t) => LoginError::Transport {
            stage,
            source: anyhow::anyhow!(t.to_string()),
        }
        .into(),
    }
}

fn response_body_snippet(resp: ureq::Response) -> String {
    let mut buf = String::new();
    let _ = resp.into_reader().take(512).read_to_string(&mut buf);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_units_convert_to_minutes() {
        assert_eq!(parse_ttl_minutes("30m").unwrap(), 30);
        assert_eq!(parse_ttl_minutes("2h").unwrap(), 120);
        assert_eq!(parse_ttl_minutes("7d").unwrap(), 7 * 24 * 60);
        assert_eq!(parse_ttl_minutes(" 60m ").unwrap(), 60);
        assert_eq!(parse_ttl_minutes("365d").unwrap(), MAX_TTL_MINUTES);
    }

    #[test]
    fn ttl_rejects_ambiguous_or_out_of_range_input() {
        // A bare number could be minutes, hours or days — ask.
        assert!(parse_ttl_minutes("7").is_err());
        assert!(parse_ttl_minutes("").is_err());
        assert!(parse_ttl_minutes("m").is_err());
        assert!(parse_ttl_minutes("2w").is_err());
        assert!(parse_ttl_minutes("-1h").is_err());
        assert!(parse_ttl_minutes("0m").is_err());
        assert!(parse_ttl_minutes("366d").is_err());
    }

    #[test]
    fn granted_duration_reads_in_the_biggest_useful_unit() {
        assert_eq!(format_minutes(0), "under a minute");
        assert_eq!(format_minutes(59), "59m");
        assert_eq!(format_minutes(60), "1h");
        assert_eq!(format_minutes(60 * 24), "1d");
        assert_eq!(format_minutes(365 * 24 * 60), "365d");
    }
}
