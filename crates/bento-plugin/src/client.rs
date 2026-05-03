//! `Client` — owns a spawned plugin process and exposes a synchronous
//! request/response API over the JSON-RPC framing.
//!
//! Threading model: one **reader thread** per plugin sits in a blocking
//! loop on the child's stdout, parsing framed messages and forwarding
//! them through an `mpsc::channel` to the main thread. The main thread
//! does the writes and uses `recv_timeout` for per-call deadlines.
//! Calls are serialised (one in-flight at a time, per the design doc
//! Decisions §3); the channel still carries notifications interleaved,
//! which `call()` drains inline by invoking `Notifier`.

use std::io::BufReader;
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::de::DeserializeOwned;
use serde::Serialize;
use tracing::{debug, warn};

use crate::framing::{self, FrameError};
use crate::protocol::{Inbound, Request, RpcError};
use crate::wire::{LogParams, Manifest};

/// Wire-protocol version this host speaks. Bumped on breaking changes
/// per design doc Versioning section.
pub const PROTOCOL_VERSION: u32 = 1;

/// Grace period after `shutdown` request before we escalate to SIGTERM.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
/// Grace period after SIGTERM before SIGKILL.
const SIGTERM_GRACE: Duration = Duration::from_secs(2);

/// Receives `notifications/log` events from the plugin while a call is
/// in flight. Implementations are invoked from the main thread (the one
/// that called `Client::call`), so they don't need to be `Sync`.
pub trait Notifier {
    fn on_log(&mut self, params: &LogParams);
}

/// Default notifier — discards everything. Useful for tests and for
/// methods that aren't expected to emit logs.
pub struct NoopNotifier;
impl Notifier for NoopNotifier {
    fn on_log(&mut self, _params: &LogParams) {}
}

#[derive(Debug)]
enum ReaderMessage {
    Inbound(Inbound),
    /// Reader hit EOF or a fatal frame error — channel will close after.
    Closed(Option<FrameError>),
}

#[derive(Debug)]
pub struct Client {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<ReaderMessage>,
    next_id: AtomicU64,
    manifest: Manifest,
    /// Held so the reader thread is joined on Drop. `Option` so we can
    /// take it during explicit shutdown.
    reader: Option<JoinHandle<()>>,
}

impl Client {
    /// Spawn `binary`, perform the `initialize` handshake, and verify the
    /// plugin's announced `adapter_id` matches `expected_id` (the suffix
    /// of the binary's PATH name). Convenience wrapper around
    /// [`Self::from_command`] with no extra arguments — what plugin
    /// discovery uses in production.
    pub fn spawn(
        binary: &Path,
        expected_id: &str,
        bento_version: &str,
        handshake_timeout: Duration,
    ) -> Result<Self> {
        let mut cmd = Command::new(binary);
        Self::from_command(&mut cmd, expected_id, bento_version, handshake_timeout)
    }

    /// Lower-level entry point: caller supplies an already-configured
    /// `Command` (useful for tests that need to pass scenario flags via
    /// argv). stdin/stdout/stderr piping is set here regardless of what
    /// the caller did.
    pub fn from_command(
        cmd: &mut Command,
        expected_id: &str,
        bento_version: &str,
        handshake_timeout: Duration,
    ) -> Result<Self> {
        let program = cmd.get_program().to_string_lossy().into_owned();
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // stderr inherited: plugin diagnostics + tool subprocess output
            // go straight to the user's terminal.
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawning plugin binary {program}"))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("plugin {program} stdin not piped"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("plugin {program} stdout not piped"))?;

        let (tx, rx) = mpsc::channel::<ReaderMessage>();
        let reader = std::thread::Builder::new()
            .name(format!("bento-plugin-reader[{}]", expected_id))
            .spawn(move || reader_loop(BufReader::new(stdout), tx))
            .context("spawning plugin reader thread")?;

        let mut client = Self {
            child,
            stdin,
            rx,
            next_id: AtomicU64::new(1),
            manifest: placeholder_manifest(),
            reader: Some(reader),
        };

        // Handshake.
        #[derive(Serialize)]
        struct InitParams<'a> {
            protocol_version: u32,
            bento_version: &'a str,
        }
        let manifest: Manifest = client
            .call(
                "initialize",
                Some(&InitParams {
                    protocol_version: PROTOCOL_VERSION,
                    bento_version,
                }),
                handshake_timeout,
                &mut NoopNotifier,
            )
            .context("plugin initialize handshake failed")?;

        if manifest.protocol_version != PROTOCOL_VERSION {
            // Best-effort kill — we're about to drop the half-initialised
            // client anyway, but be explicit so the user sees a clean
            // teardown message.
            let _ = client.child.kill();
            bail!(
                "plugin {program} announced protocol_version={}, host speaks {} — refusing to load",
                manifest.protocol_version,
                PROTOCOL_VERSION
            );
        }
        if manifest.adapter_id != expected_id {
            let _ = client.child.kill();
            bail!(
                "plugin binary {program} announced adapter_id={:?} but its PATH name expects {:?} — refusing to load",
                manifest.adapter_id,
                expected_id
            );
        }

        client.manifest = manifest;
        debug!(
            adapter = %client.manifest.adapter_id,
            display = %client.manifest.display_name,
            "plugin handshake ok"
        );
        Ok(client)
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// Issue a request, drain any notifications that arrive before the
    /// response, return the typed `result`.
    pub fn call<P, R, N>(
        &mut self,
        method: &str,
        params: Option<&P>,
        timeout: Duration,
        notifier: &mut N,
    ) -> Result<R>
    where
        P: Serialize,
        R: DeserializeOwned,
        N: Notifier + ?Sized,
    {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let req = Request::new(id, method, params);
        let body = serde_json::to_string(&req).context("serialising plugin request")?;
        framing::write_message(&mut self.stdin, &body)
            .with_context(|| format!("writing {method} request to plugin"))?;

        let deadline = Instant::now() + timeout;
        loop {
            let remaining = match deadline.checked_duration_since(Instant::now()) {
                Some(d) if !d.is_zero() => d,
                _ => {
                    self.kill_child();
                    bail!(
                        "plugin {} timed out after {:?} waiting for response to {method:?}",
                        self.manifest.adapter_id,
                        timeout
                    );
                }
            };

            match self.rx.recv_timeout(remaining) {
                Ok(ReaderMessage::Inbound(Inbound::Notification(n))) => {
                    if n.method == "notifications/log" {
                        if let Some(params) = n.params {
                            match serde_json::from_value::<LogParams>(params) {
                                Ok(p) => notifier.on_log(&p),
                                Err(e) => warn!(
                                    error = %e,
                                    "plugin sent malformed notifications/log payload"
                                ),
                            }
                        }
                    } else {
                        debug!(method = %n.method, "ignoring unknown notification");
                    }
                }
                Ok(ReaderMessage::Inbound(Inbound::Response(resp))) => {
                    if resp.id != id {
                        // Calls are serialised, so an out-of-order id is a
                        // plugin protocol violation.
                        self.kill_child();
                        bail!(
                            "plugin {} sent response for id={} while waiting on id={}",
                            self.manifest.adapter_id,
                            resp.id,
                            id
                        );
                    }
                    if let Some(err) = resp.error {
                        return Err(plugin_error(&self.manifest.adapter_id, method, err));
                    }
                    let raw = resp.result.unwrap_or(serde_json::Value::Null);
                    return serde_json::from_value::<R>(raw).with_context(|| {
                        format!(
                            "deserialising {method} response from plugin {}",
                            self.manifest.adapter_id
                        )
                    });
                }
                Ok(ReaderMessage::Closed(maybe_err)) => {
                    let detail = maybe_err.map(|e| format!(": {e}")).unwrap_or_default();
                    bail!(
                        "plugin {} stdout closed before responding to {method:?}{detail}",
                        self.manifest.adapter_id
                    );
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.kill_child();
                    bail!(
                        "plugin {} timed out after {:?} waiting for response to {method:?}",
                        self.manifest.adapter_id,
                        timeout
                    );
                }
                Err(RecvTimeoutError::Disconnected) => {
                    bail!(
                        "plugin {} reader channel disconnected mid-call ({method:?})",
                        self.manifest.adapter_id
                    );
                }
            }
        }
    }

    /// Best-effort graceful shutdown: send the `shutdown` request, wait
    /// 2s; if the child is still alive, SIGTERM (Unix) and wait another
    /// 2s; if still alive, SIGKILL.
    pub fn shutdown(mut self) -> Result<()> {
        // Issue shutdown. Ignore errors — we're tearing down regardless.
        let _ = self.call::<(), serde_json::Value, NoopNotifier>(
            "shutdown",
            None,
            SHUTDOWN_GRACE,
            &mut NoopNotifier,
        );

        // Wait for the child to exit naturally.
        if wait_with_grace(&mut self.child, SHUTDOWN_GRACE) {
            self.join_reader();
            return Ok(());
        }

        warn!(
            adapter = %self.manifest.adapter_id,
            "plugin did not exit after shutdown request; sending SIGTERM"
        );
        send_sigterm(&self.child);
        if wait_with_grace(&mut self.child, SIGTERM_GRACE) {
            self.join_reader();
            return Ok(());
        }

        warn!(
            adapter = %self.manifest.adapter_id,
            "plugin did not exit after SIGTERM; sending SIGKILL"
        );
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.join_reader();
        Ok(())
    }

    fn kill_child(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn join_reader(&mut self) {
        if let Some(handle) = self.reader.take() {
            // Best-effort: reader thread should be unwinding now that the
            // child is dead. Don't propagate panics.
            let _ = handle.join();
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // If the user didn't call `shutdown()`, kill hard. We don't have
        // 4s of grace to spend in a destructor.
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.join_reader();
    }
}

fn reader_loop<R: std::io::BufRead>(mut r: R, tx: mpsc::Sender<ReaderMessage>) {
    loop {
        match framing::read_message(&mut r) {
            Ok(body) => match serde_json::from_str::<Inbound>(&body) {
                Ok(msg) => {
                    if tx.send(ReaderMessage::Inbound(msg)).is_err() {
                        // Main thread dropped the receiver — we're done.
                        return;
                    }
                }
                Err(e) => {
                    warn!(error = %e, body_preview = %preview(&body), "plugin sent unparseable JSON");
                    // Don't try to recover — protocol is broken.
                    let _ = tx.send(ReaderMessage::Closed(None));
                    return;
                }
            },
            Err(FrameError::UnexpectedEof { .. }) => {
                let _ = tx.send(ReaderMessage::Closed(None));
                return;
            }
            Err(e) => {
                let _ = tx.send(ReaderMessage::Closed(Some(e)));
                return;
            }
        }
    }
}

fn preview(body: &str) -> String {
    const MAX: usize = 120;
    if body.len() <= MAX {
        body.to_string()
    } else {
        format!("{}…", &body[..MAX])
    }
}

fn plugin_error(adapter: &str, method: &str, err: RpcError) -> anyhow::Error {
    anyhow!(
        "plugin {adapter} returned error for {method}: code={} msg={:?}",
        err.code,
        err.message
    )
}

fn placeholder_manifest() -> Manifest {
    Manifest {
        protocol_version: 0,
        adapter_id: String::new(),
        display_name: String::new(),
        fingerprint_files: Vec::new(),
        default_tasks: Vec::new(),
        capabilities: Default::default(),
        diagnostic_hooks: std::collections::BTreeMap::new(),
    }
}

/// Poll `child.try_wait` until it exits or `grace` elapses. Returns
/// `true` if the child exited within the grace period.
fn wait_with_grace(child: &mut Child, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    let mut backoff = Duration::from_millis(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(backoff.min(deadline - Instant::now()));
                backoff = (backoff * 2).min(Duration::from_millis(100));
            }
            Err(_) => return false,
        }
    }
}

#[cfg(unix)]
fn send_sigterm(child: &Child) {
    // child.id() returns Option<u32> on stable since 1.74? It's a u32 here.
    let pid = child.id() as i32;
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn send_sigterm(child: &Child) {
    // No real SIGTERM on Windows — the design doc accepts this.
    // SIGKILL escalation will still happen next.
    let _ = child;
}

// Tests that exercise a real spawned subprocess live in `tests/fixture.rs`
// because Cargo only sets `CARGO_BIN_EXE_<name>` for integration tests.
