//! Wire-level smoke test: spawn the real `bento-mcp` binary, speak
//! stdio JSON-RPC at it, and assert the handshake, the tool catalogue,
//! and one tool result.
//!
//! Everything here is read-only (`prime` never executes tasks), so the
//! test needs no language toolchains — only a workspace fixture and a
//! throwaway cache dir.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// Every tool the server is expected to expose. Kept explicit so
/// adding or dropping a tool is a deliberate edit here too — the tool
/// surface is a published contract for agents.
const EXPECTED_TOOLS: &[&str] = &[
    "artifacts",
    "box_list",
    "build",
    "check",
    "ci",
    "deploy",
    "dish_list",
    "doctor",
    "install",
    "lint",
    "notify",
    "plan",
    "prime",
    "schema",
    "test",
    "why",
];

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("bento-mcp is two levels below the workspace root")
        .join("tests/e2e/fixtures/monorepo-go-node")
}

struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl Server {
    fn spawn(cache_dir: &std::path::Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_bento-mcp"))
            .arg("--workspace")
            .arg(fixture_root())
            .env("BENTO_CACHE_DIR", cache_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn bento-mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        }
    }

    /// Send a request, then read lines until the matching response
    /// arrives (notifications may interleave).
    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.next_id += 1;
        let id = self.next_id;
        let req = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        });
        writeln!(self.stdin, "{req}").expect("write request");
        self.stdin.flush().unwrap();

        loop {
            let mut line = String::new();
            let n = self.stdout.read_line(&mut line).expect("read response");
            assert!(n > 0, "bento-mcp closed stdout before answering {method}");
            let msg: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => panic!("non-JSON line on the wire: {e}: {line}"),
            };
            if msg.get("id").and_then(|v| v.as_i64()) == Some(id) {
                assert!(
                    msg.get("error").is_none(),
                    "{method} returned a protocol error: {msg}"
                );
                return msg["result"].clone();
            }
        }
    }

    fn notify(&mut self, method: &str) {
        let msg = serde_json::json!({"jsonrpc": "2.0", "method": method, "params": {}});
        writeln!(self.stdin, "{msg}").expect("write notification");
        self.stdin.flush().unwrap();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn initialize_lists_annotated_tools_and_primes_the_workspace() {
    let cache = tempfile::tempdir().unwrap();
    let mut server = Server::spawn(cache.path());

    let init = server.request(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "bento-mcp-test", "version": "0"},
        }),
    );
    assert_eq!(init["serverInfo"]["name"], "bento-mcp");
    assert!(
        init["instructions"]
            .as_str()
            .is_some_and(|s| s.contains("prime")),
        "instructions should point agents at `prime` first: {init}"
    );
    server.notify("notifications/initialized");

    let tools = server.request("tools/list", serde_json::json!({}));
    let listed: Vec<&str> = tools["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().expect("tool name"))
        .collect();
    let mut sorted = listed.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, EXPECTED_TOOLS, "tool catalogue drifted");

    // Annotations drive client-side confirmation prompts — a tool
    // without them is treated as "unknown risk" (or worse, harmless).
    for tool in tools["tools"].as_array().unwrap() {
        let annotations = &tool["annotations"];
        assert!(
            annotations.get("readOnlyHint").is_some()
                || annotations.get("destructiveHint").is_some(),
            "tool {} has no annotations: {tool}",
            tool["name"]
        );
    }

    let result = server.request(
        "tools/call",
        serde_json::json!({"name": "prime", "arguments": {}}),
    );
    assert_eq!(result["isError"], serde_json::json!(false));
    let out = &result["structuredContent"];
    assert!(
        out["workspace_root"]
            .as_str()
            .unwrap()
            .ends_with("monorepo-go-node"),
        "unexpected workspace_root: {out}"
    );
    let dishes: Vec<&str> = out["dishes"]
        .as_array()
        .expect("dishes array")
        .iter()
        .map(|d| d["name"].as_str().unwrap())
        .collect();
    assert!(
        dishes.contains(&"backend") && dishes.contains(&"frontend"),
        "expected both fixture dishes, got {dishes:?}"
    );
    assert!(
        !out["recommended_next"].as_array().unwrap().is_empty(),
        "prime must always recommend a next verb: {out}"
    );
}

#[test]
fn unknown_target_is_a_tool_result_not_a_protocol_error() {
    let cache = tempfile::tempdir().unwrap();
    let mut server = Server::spawn(cache.path());
    server.request(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "bento-mcp-test", "version": "0"},
        }),
    );
    server.notify("notifications/initialized");

    // `request` asserts the absence of a JSON-RPC `error` member, so
    // this alone proves failures come back as results.
    let result = server.request(
        "tools/call",
        serde_json::json!({"name": "plan", "arguments": {"target": "nope"}}),
    );
    assert_eq!(result["isError"], serde_json::json!(true));
    let envelope = &result["structuredContent"];
    assert_eq!(envelope["kind"], "target_not_found");
    assert!(
        envelope["next_steps"]
            .as_array()
            .is_some_and(|s| !s.is_empty()),
        "classified errors carry recovery steps: {envelope}"
    );
}
