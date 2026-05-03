//! Spawned-subprocess tests for `Client`.
//!
//! Each test runs the `bento-plugin-fixture` binary in a different
//! scenario (passed as `argv[1]`) and asserts the host's behaviour.

use std::process::Command;
use std::time::Duration;

use bento_plugin::wire::{LogLevel, LogParams, LogStream};
use bento_plugin::{Client, NoopNotifier, Notifier, PROTOCOL_VERSION};

fn fixture_path() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_bento-plugin-fixture"))
}

fn spawn_with(scenario: &str) -> anyhow::Result<Client> {
    let mut cmd = Command::new(fixture_path());
    cmd.arg(scenario);
    Client::from_command(&mut cmd, "fixture", "test/0.0.0", Duration::from_secs(5))
}

#[test]
fn handshake_succeeds_with_ok_fixture() {
    let client = spawn_with("ok").expect("handshake should succeed");
    assert_eq!(client.manifest().adapter_id, "fixture");
    assert_eq!(client.manifest().protocol_version, PROTOCOL_VERSION);
    assert_eq!(client.manifest().fingerprint_files, vec!["fixture.toml"]);
    client.shutdown().unwrap();
}

#[test]
fn wrong_protocol_version_is_rejected() {
    let err = spawn_with("wrong-version").expect_err("expected handshake to be rejected");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("protocol_version"),
        "expected version error, got: {msg}"
    );
}

#[test]
fn wrong_adapter_id_is_rejected() {
    let err = spawn_with("wrong-id").expect_err("expected handshake to be rejected");
    let msg = format!("{err:#}");
    assert!(msg.contains("adapter_id"), "expected id error, got: {msg}");
}

#[test]
fn timeout_kills_child() {
    let mut client = spawn_with("hang-after-init").unwrap();
    let err = client
        .call::<(), serde_json::Value, NoopNotifier>(
            "hang",
            None,
            Duration::from_millis(150),
            &mut NoopNotifier,
        )
        .expect_err("expected timeout");
    assert!(format!("{err:#}").contains("timed out"));
    drop(client); // Drop reaps the killed child.
}

#[test]
fn child_crash_mid_call_is_clear_error() {
    let mut client = spawn_with("crash-after-init").unwrap();
    let err = client
        .call::<(), serde_json::Value, NoopNotifier>(
            "trigger-crash",
            None,
            Duration::from_secs(5),
            &mut NoopNotifier,
        )
        .expect_err("expected crash error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("stdout closed") || msg.contains("disconnected"),
        "expected channel-closed error, got: {msg}"
    );
}

struct CapturingNotifier(std::sync::Arc<std::sync::Mutex<Vec<LogParams>>>);
impl Notifier for CapturingNotifier {
    fn on_log(&mut self, params: &LogParams) {
        self.0.lock().unwrap().push(params.clone());
    }
}

#[test]
fn notifications_drain_to_notifier_inline() {
    let mut client = spawn_with("emit-logs").unwrap();
    let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut notifier = CapturingNotifier(log.clone());
    let _: serde_json::Value = client
        .call(
            "emit-and-return",
            None::<&()>,
            Duration::from_secs(5),
            &mut notifier,
        )
        .unwrap();
    let captured = log.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0].level, LogLevel::Info);
    assert_eq!(captured[0].stream, Some(LogStream::Stdout));
    assert_eq!(captured[1].level, LogLevel::Warn);
    drop(captured);
    client.shutdown().unwrap();
}
