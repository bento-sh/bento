//! Test fixture for `bento-plugin` integration-style unit tests.
//!
//! Usage: invoked by tests with one of these scenarios as `argv[1]`:
//!
//!  * `ok`              — correct handshake, responds to shutdown.
//!  * `wrong-version`   — handshake announces protocol_version=99.
//!  * `wrong-id`        — handshake announces adapter_id="not-fixture".
//!  * `hang-after-init` — handshake ok, then reads but never responds.
//!  * `crash-after-init`— handshake ok, then exits on the next request.
//!  * `emit-logs`       — handshake ok, next request → 2 log notifs + result.
//!
//! Not part of the published bento binary surface — only built so tests
//! can `env!("CARGO_BIN_EXE_bento-plugin-fixture")`.

use std::io::{BufReader, Write};

use bento_plugin::framing;
use serde_json::{json, Value};

fn main() {
    let scenario = std::env::args().nth(1).unwrap_or_else(|| "ok".into());

    let stdin = std::io::stdin();
    let mut r = BufReader::new(stdin.lock());
    let mut w = std::io::stdout();

    // First message must be `initialize`.
    let init_body = framing::read_message(&mut r).expect("read initialize");
    let init: Value = serde_json::from_str(&init_body).expect("parse initialize");
    let init_id = init["id"].as_u64().expect("initialize must have id");

    let manifest = match scenario.as_str() {
        "wrong-version" => json!({
            "protocol_version": 99,
            "adapter_id": "fixture",
            "display_name": "fixture",
            "fingerprint_files": [],
            "default_tasks": []
        }),
        "wrong-id" => json!({
            "protocol_version": 1,
            "adapter_id": "not-fixture",
            "display_name": "fixture",
            "fingerprint_files": [],
            "default_tasks": []
        }),
        _ => json!({
            "protocol_version": 1,
            "adapter_id": "fixture",
            "display_name": "fixture",
            "fingerprint_files": ["fixture.toml"],
            "default_tasks": [
                {"name": "test", "run": "echo ok"}
            ]
        }),
    };
    write_response(&mut w, init_id, manifest);

    // Scenarios that bail before second message — host kills us when it
    // notices the bad handshake.
    if scenario == "wrong-version" || scenario == "wrong-id" {
        loop_until_killed(&mut r);
    }

    // Subsequent messages.
    loop {
        let body = match framing::read_message(&mut r) {
            Ok(b) => b,
            Err(_) => return, // host closed stdin → exit cleanly
        };
        let msg: Value = serde_json::from_str(&body).expect("parse request");
        let id = msg["id"].as_u64();
        let method = msg["method"].as_str().unwrap_or("").to_string();

        match (scenario.as_str(), method.as_str()) {
            ("hang-after-init", _) => {
                // Read the request, never respond — host's call() should time out.
                loop_until_killed(&mut r);
            }
            ("crash-after-init", _) => {
                // Exit before responding.
                std::process::exit(7);
            }
            ("emit-logs", "emit-and-return") => {
                if let Some(req_id) = id {
                    write_notification(
                        &mut w,
                        "notifications/log",
                        json!({"level": "info", "stream": "stdout", "message": "first"}),
                    );
                    write_notification(
                        &mut w,
                        "notifications/log",
                        json!({"level": "warn", "message": "second"}),
                    );
                    write_response(&mut w, req_id, json!(null));
                }
            }
            (_, "shutdown") => {
                if let Some(req_id) = id {
                    write_response(&mut w, req_id, json!(null));
                }
                return;
            }
            _ => {
                // Unknown method: respond with a generic error so the
                // host's call() returns rather than timing out.
                if let Some(req_id) = id {
                    write_error(
                        &mut w,
                        req_id,
                        -32601,
                        &format!("method not found: {method}"),
                    );
                }
            }
        }
    }
}

fn loop_until_killed<R: std::io::BufRead>(r: &mut R) -> ! {
    loop {
        // Drain stdin so the host's writes don't block, but never reply.
        if framing::read_message(r).is_err() {
            std::process::exit(0);
        }
    }
}

fn write_response<W: Write>(w: &mut W, id: u64, result: Value) {
    let body = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
    framing::write_message(w, &body).expect("write response");
}

fn write_error<W: Write>(w: &mut W, id: u64, code: i32, message: &str) {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    })
    .to_string();
    framing::write_message(w, &body).expect("write error");
}

fn write_notification<W: Write>(w: &mut W, method: &str, params: Value) {
    let body = json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string();
    framing::write_message(w, &body).expect("write notification");
}
