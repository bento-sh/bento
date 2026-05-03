//! Integration test for the bento:// remote cache client.
//!
//! Uses `wiremock` as a local HTTP server that pretends to be
//! `cache.bento.build`. Exercises HEAD/GET/PUT with real HTTP + Bearer
//! auth.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use bento_cache::{BearerRemote, CacheKey, RemoteCache};
use wiremock::matchers::{bearer_token, method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const TOKEN: &str = "test.jwt.token";
const HEX: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
const BODY: &[u8] = b"fake-tar-bundle-contents";

/// wiremock's `MockServer::start()` requires a tokio runtime; spin up a
/// dedicated multi-thread one so the BearerRemote's own single-thread
/// runtime doesn't conflict.
fn spawn_server() -> (MockServer, tokio::runtime::Runtime) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let server = rt.block_on(MockServer::start());
    (server, rt)
}

fn make_remote(base: &str) -> BearerRemote {
    BearerRemote::from_http_base(base, TOKEN.to_string()).unwrap()
}

#[test]
fn head_returns_true_on_200() {
    let (server, rt) = spawn_server();
    rt.block_on(
        Mock::given(method("HEAD"))
            .and(path(format!("/cache/{HEX}")))
            .and(bearer_token(TOKEN))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server),
    );

    let remote = make_remote(&server.uri());
    let key = CacheKey::from_hex(HEX);
    assert!(remote.has(&key));
}

#[test]
fn head_returns_false_on_404() {
    let (server, rt) = spawn_server();
    rt.block_on(
        Mock::given(method("HEAD"))
            .and(path(format!("/cache/{HEX}")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server),
    );

    let remote = make_remote(&server.uri());
    let key = CacheKey::from_hex(HEX);
    assert!(!remote.has(&key));
}

#[test]
fn head_returns_false_on_401() {
    let (server, rt) = spawn_server();
    rt.block_on(
        Mock::given(method("HEAD"))
            .and(path(format!("/cache/{HEX}")))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server),
    );

    let remote = make_remote(&server.uri());
    let key = CacheKey::from_hex(HEX);
    assert!(!remote.has(&key));
}

#[test]
fn get_writes_body_on_200() {
    let (server, rt) = spawn_server();
    rt.block_on(
        Mock::given(method("GET"))
            .and(path(format!("/cache/{HEX}")))
            .and(bearer_token(TOKEN))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(BODY))
            .mount(&server),
    );

    let remote = make_remote(&server.uri());
    let key = CacheKey::from_hex(HEX);
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("bundle.tar");

    assert!(remote.get(&key, &dest).unwrap());
    assert_eq!(std::fs::read(&dest).unwrap(), BODY);
}

#[test]
fn get_returns_false_on_404() {
    let (server, rt) = spawn_server();
    rt.block_on(
        Mock::given(method("GET"))
            .and(path(format!("/cache/{HEX}")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server),
    );

    let remote = make_remote(&server.uri());
    let key = CacheKey::from_hex(HEX);
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("bundle.tar");

    assert!(!remote.get(&key, &dest).unwrap());
    assert!(!dest.exists());
}

#[test]
fn put_succeeds_on_2xx() {
    let (server, rt) = spawn_server();
    rt.block_on(
        Mock::given(method("PUT"))
            .and(path(format!("/cache/{HEX}")))
            .and(bearer_token(TOKEN))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server),
    );

    let remote = make_remote(&server.uri());
    let key = CacheKey::from_hex(HEX);
    let tmp = tempfile::tempdir().unwrap();
    let bundle = tmp.path().join("bundle.tar");
    std::fs::write(&bundle, BODY).unwrap();

    remote.put(&key, &bundle).unwrap();
}

#[test]
fn put_surfaces_over_quota_413() {
    let (server, rt) = spawn_server();
    rt.block_on(
        Mock::given(method("PUT"))
            .and(path(format!("/cache/{HEX}")))
            .respond_with(ResponseTemplate::new(413))
            .mount(&server),
    );

    let remote = make_remote(&server.uri());
    let key = CacheKey::from_hex(HEX);
    let tmp = tempfile::tempdir().unwrap();
    let bundle = tmp.path().join("bundle.tar");
    std::fs::write(&bundle, BODY).unwrap();

    let err = remote.put(&key, &bundle).unwrap_err();
    assert!(err.to_string().contains("413"), "got: {err}");
    assert!(err.to_string().contains("over quota"), "got: {err}");
}

/// Custom Respond impl counting calls: first returns 503, second 200.
/// Confirms the single-retry path fires on a transient 5xx.
struct RetryOnce {
    count: Arc<AtomicU32>,
}

impl Respond for RetryOnce {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let n = self.count.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            ResponseTemplate::new(503)
        } else {
            ResponseTemplate::new(200).set_body_bytes(BODY)
        }
    }
}

#[test]
fn get_retries_once_on_5xx() {
    let (server, rt) = spawn_server();
    let count = Arc::new(AtomicU32::new(0));
    rt.block_on(
        Mock::given(method("GET"))
            .and(path(format!("/cache/{HEX}")))
            .respond_with(RetryOnce {
                count: count.clone(),
            })
            .mount(&server),
    );

    let remote = make_remote(&server.uri());
    let key = CacheKey::from_hex(HEX);
    let tmp = tempfile::tempdir().unwrap();
    let dest = tmp.path().join("bundle.tar");

    assert!(remote.get(&key, &dest).unwrap());
    assert_eq!(count.load(Ordering::SeqCst), 2, "expected one retry");
    assert_eq!(std::fs::read(&dest).unwrap(), BODY);
}

#[test]
fn url_with_prefix_routes_correctly() {
    let (server, rt) = spawn_server();
    rt.block_on(
        Mock::given(method("HEAD"))
            .and(path(format!("/team/xyz/cache/{HEX}")))
            .and(bearer_token(TOKEN))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server),
    );

    let base_with_prefix = format!("{}/team/xyz", server.uri());
    let remote = BearerRemote::from_http_base(base_with_prefix, TOKEN.to_string()).unwrap();
    let key = CacheKey::from_hex(HEX);
    assert!(remote.has(&key));
}
