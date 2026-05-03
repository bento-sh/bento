//! Reference subprocess plugin for bento — the 'noop' language.
//!
//! Self-contained. Depends only on `serde` / `serde_json` / `std` so that
//! it doubles as a from-scratch implementation guide for a new plugin
//! author. NO dependency on `bento-plugin` — that would be cheating.
//!
//! Behaviour:
//! - `id` = "noop"
//! - `detect(dir)` matches when `dir/noop.toml` exists.
//! - `default_tasks` provides `build` (echoes), `test` (`true`), `lint` (`true`).
//! - `install` is a no-op, but emits one log notification so the host can
//!   prove it routes them.
//! - `requiredToolchain` always returns `null`.
//!
//! Wire format: LSP-style `Content-Length` framing over stdin/stdout,
//! JSON-RPC 2.0 message bodies.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use serde_json::{json, Value};

const PROTOCOL_VERSION: u32 = 1;
const ADAPTER_ID: &str = "noop";

fn main() {
    let stdin = std::io::stdin();
    let mut r = BufReader::new(stdin.lock());
    let mut w = std::io::stdout();

    loop {
        let body = match read_message(&mut r) {
            Ok(b) => b,
            Err(_) => return, // host closed stdin → exit
        };
        let msg: Value = match serde_json::from_str(&body) {
            Ok(v) => v,
            Err(_) => continue, // skip garbage rather than crash
        };
        let id = msg["id"].as_u64();
        let method = msg["method"].as_str().unwrap_or("");

        match method {
            "initialize" => {
                if let Some(req_id) = id {
                    write_response(
                        &mut w,
                        req_id,
                        json!({
                            "protocol_version": PROTOCOL_VERSION,
                            "adapter_id": ADAPTER_ID,
                            "display_name": "noop (reference plugin)",
                            "fingerprint_files": ["noop.toml"],
                            "default_tasks": [
                                {"name": "build", "run": "echo noop build", "inputs": ["**/*"]},
                                {"name": "test",  "run": "true"},
                                {"name": "lint",  "run": "true"}
                            ],
                            "capabilities": {
                                "detect": true,
                                "required_toolchain": false,
                                "resolved_toolchain_fingerprint": false,
                                "install": true
                            },
                            "diagnostic_hooks": {
                                "lint": {
                                    "rerun": {
                                        "kind": "replace",
                                        "command": "false"
                                    },
                                    "parser": "plugin"
                                }
                            }
                        }),
                    );
                }
            }
            "detect" => {
                if let Some(req_id) = id {
                    let dir = msg["params"]["dir"].as_str().unwrap_or("");
                    let matches = !dir.is_empty() && Path::new(dir).join("noop.toml").is_file();
                    write_response(&mut w, req_id, json!({"matches": matches}));
                }
            }
            "requiredToolchain" => {
                if let Some(req_id) = id {
                    write_response(&mut w, req_id, Value::Null);
                }
            }
            "install" => {
                if let Some(req_id) = id {
                    let dish = msg["params"]["dish_name"].as_str().unwrap_or("?");
                    write_notification(
                        &mut w,
                        "notifications/log",
                        json!({
                            "level": "info",
                            "message": format!("noop install for dish {dish} (no-op)\n"),
                        }),
                    );
                    write_response(&mut w, req_id, Value::Null);
                }
            }
            "parseDiagnostics" => {
                if let Some(req_id) = id {
                    let task = msg["params"]["task_name"].as_str().unwrap_or("?");
                    // Reference implementation: emit one hardcoded
                    // diagnostic to demonstrate the round-trip. Real
                    // plugins would parse params.stdout / params.stderr
                    // into their tool's actual diagnostic shape.
                    write_response(
                        &mut w,
                        req_id,
                        json!({
                            "diagnostics": [{
                                "file": "noop.toml",
                                "line": 1,
                                "severity": "warning",
                                "message": format!("noop plugin reporting on {task}"),
                                "source": ADAPTER_ID,
                                "rule": "noop-demo"
                            }]
                        }),
                    );
                }
            }
            "shutdown" => {
                if let Some(req_id) = id {
                    write_response(&mut w, req_id, Value::Null);
                }
                return;
            }
            other => {
                if let Some(req_id) = id {
                    write_error(
                        &mut w,
                        req_id,
                        -32601,
                        &format!("method not found: {other}"),
                    );
                }
            }
        }
    }
}

// ── Framing (Content-Length) ─────────────────────────────────────────

fn read_message<R: BufRead>(r: &mut R) -> std::io::Result<String> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = r.read_line(&mut line)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "headers",
            ));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if let Some((name, value)) = trimmed.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().ok();
            }
        }
    }
    let len = content_length.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "missing Content-Length")
    })?;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn write_response<W: Write>(w: &mut W, id: u64, result: Value) {
    let body = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
    write_framed(w, &body);
}

fn write_error<W: Write>(w: &mut W, id: u64, code: i32, message: &str) {
    let body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message},
    })
    .to_string();
    write_framed(w, &body);
}

fn write_notification<W: Write>(w: &mut W, method: &str, params: Value) {
    let body = json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string();
    write_framed(w, &body);
}

fn write_framed<W: Write>(w: &mut W, body: &str) {
    let bytes = body.as_bytes();
    let _ = write!(w, "Content-Length: {}\r\n\r\n", bytes.len());
    let _ = w.write_all(bytes);
    let _ = w.flush();
}
