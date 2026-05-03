# Writing a bento plugin

bento ships thirteen built-in adapters (Go, Cargo, Python (pip + uv),
Ruby, PHP, Maven, Gradle, npm, pnpm, yarn, Bun, Deno). To teach bento
about another language without forking the binary, ship a **subprocess
plugin**: a small executable on `$PATH` that speaks JSON-RPC 2.0 over
stdio.

A complete reference implementation lives in
[`examples/bento-adapter-noop`](../examples/bento-adapter-noop). It depends
only on `serde` / `std` to make the point that the protocol is buildable
from anywhere — no `bento-plugin` import.

## Discovery

bento walks `$PATH` looking for executables matching `bento-adapter-*`.
The suffix is the adapter id and **must match** what the binary
announces in its `initialize` response.

| Binary on `$PATH`            | Adapter id |
| ---------------------------- | ---------- |
| `bento-adapter-zig`          | `zig`      |
| `bento-adapter-erlang`       | `erlang`   |
| `bento-adapter-noop`         | `noop`     |

A workspace can opt out of plugins via `bento.toml`:

```toml
[plugins]
disable   = ["zig"]              # never load these
allowlist = ["erlang", "elixir"] # if set, load ONLY these
```

Conflict rules:

- **Built-ins always win.** A plugin claiming `go` is loaded but ignored
  by `by_id`/`detect` because the built-in was registered first.
- **Between plugins**, first entry on `$PATH` wins; later duplicates are
  skipped with a warning. Order is deterministic — `$PATH` order.

## Wire protocol

Every message is a header block (`Content-Length: <n>\r\n\r\n`) followed
by `<n>` UTF-8 bytes of JSON-RPC 2.0. (Length-framing rather than NDJSON:
JSON values can contain literal newlines, every language has an LSP
client/server library that already implements this, and it's trivially
debuggable with a hex dump.) Plugin's `stdin` receives requests from
bento. Plugin's `stdout` carries responses and notifications. Plugin's
`stderr` is inherited and goes straight to the user's terminal — useful
for diagnostics; **do NOT** use it for protocol messages.

### Lifecycle

1. bento spawns the plugin once per `bento` invocation, after CLI parsing
   and before workspace discovery.
2. bento sends `initialize`. Plugin responds with its manifest.
3. During the run, bento sends `detect`, `requiredToolchain`,
   `resolvedToolchainFingerprint`, and `install` as needed; the plugin
   may emit `notifications/log` events at any time during a long-running
   call.
4. On bento exit (success or error), bento sends `shutdown`, waits up to
   2s for a clean exit, then SIGTERM, then SIGKILL.

A plugin spawned but never queried still gets `initialize` + `shutdown` —
the handshake cost is unavoidable because we need the manifest to know
what the plugin can do.

## Methods

The host enforces a per-call timeout: 30s for queries, 30 minutes for
`install`. A plugin that wedges past these is killed and the run
continues without it.

### `initialize` (request)

```json
// host → plugin
{"jsonrpc":"2.0","id":1,"method":"initialize",
 "params":{"protocol_version":1,"bento_version":"0.1.0"}}
```

```json
// plugin → host (the manifest)
{"jsonrpc":"2.0","id":1,"result":{
  "protocol_version": 1,
  "adapter_id": "noop",
  "display_name": "noop (reference plugin)",
  "fingerprint_files": ["noop.toml"],
  "default_tasks": [
    {"name": "build", "run": "echo noop build", "inputs": ["**/*"]},
    {"name": "test",  "run": "true"}
  ],
  "capabilities": {
    "detect": true,
    "required_toolchain": false,
    "resolved_toolchain_fingerprint": false,
    "install": true
  }
}}
```

`capabilities` lets a minimal plugin opt out of methods it doesn't care
about — bento will treat the missing capability as the trait default
(e.g. `required_toolchain: false` → no toolchain version goes into the
cache key, only the declared adapter id does).

If `protocol_version` doesn't match what bento speaks, or if
`adapter_id` doesn't match the binary's `bento-adapter-<id>` suffix, the
plugin is rejected with a clear error and skipped.

### `detect` (request)

```json
{"method": "detect", "params": {"dir": "/abs/path/to/dish"}}
→ {"result": {"matches": true}}
```

Cheap. Called once per `(plugin, dish)` during workspace discovery.
Don't read source content — just check for marker files.

### `requiredToolchain` (request)

```json
{"method": "requiredToolchain", "params": {"dir": "/abs/path"}}
→ {"result": {"tool": "erlang", "version": "26.2"}}    // or null
```

Return `null` when the project doesn't pin a version.

### `resolvedToolchainFingerprint` (no params)

```json
{"method": "resolvedToolchainFingerprint"}
→ {"result": "Erlang/OTP 26 [erts-14.2.1] ..."}        // or null
```

Should run `<tool> --version`-style probing. Bento memoises the result
across the whole run, so this is called **at most once** per process.
The string is opaque; bento just hashes it into the cache key.

### `install` (long-running)

```json
{"method": "install", "params": {"dish_dir": "/abs", "dish_name": "api"}}
```

While installing, the plugin can stream progress as `notifications/log`:

```json
{"method": "notifications/log",
 "params": {"level":"info","stream":"stdout","message":"==> Fetching deps\n"}}
```

`level ∈ {"trace","debug","info","warn","error"}`. `stream ∈ {"stdout","stderr"}`
when forwarding tool subprocess output (bento prints it to the matching
channel verbatim); omit `stream` for plugin-internal logs (bento routes
them through `tracing` with the adapter id as a target tag).

Final response on success: `{"result": null}`. On failure, return a
JSON-RPC error with code `2001`:

```json
{"error": {"code": 2001, "message": "rebar3 compile failed",
           "data": {"exit_status": 1, "command": "rebar3 compile"}}}
```

### `shutdown`

```json
{"method": "shutdown"} → {"result": null}
```

Plugin should close stdout and exit. If it doesn't exit within 2s, bento
sends SIGTERM; if it still doesn't, SIGKILL.

### `parseDiagnostics` (optional)

Plugins that declare `diagnostic_hooks` in their manifest with a
`parser: "plugin"` entry receive this method when bento needs to turn
captured tool output into structured diagnostics. Plugins that don't
declare any hooks never see this call.

```json
// host → plugin
{"method": "parseDiagnostics", "params": {
  "task_name": "lint",
  "stdout": "<captured tool stdout>",
  "stderr": "<captured tool stderr>",
  "exit_status": 1,
  "dish_dir": "/abs/path/to/dish",
  "workspace_root": "/abs/path/to/workspace"
}}
```

```json
// plugin → host
{"result": {
  "diagnostics": [{
    "file": "src/foo.erl",
    "line": 12,
    "col": 5,
    "severity": "error",
    "message": "function my_fun/2 undefined",
    "rule": "L1234",
    "source": "rebar3"
  }]
}}
```

`severity ∈ {"error","warning","info","hint"}`. Paths in `file` should be
**relative to `workspace_root`** so agents can read them directly. If
your tool emits absolute or dish-relative paths, normalise before
returning.

### Diagnostic hooks (manifest)

Plugins declare per-task diagnostic capability in the `initialize`
response under `diagnostic_hooks`:

```json
{
  "result": {
    "protocol_version": 1,
    "adapter_id": "erlang",
    "...": "...",
    "diagnostic_hooks": {
      "lint": {
        "rerun": { "kind": "append_args", "args": ["--format", "json"] },
        "parser": "plugin"
      },
      "build": {
        "rerun": { "kind": "replace", "command": "rebar3 compile --json" },
        "parser": "plugin"
      }
    }
  }
}
```

Per task name, declare:

- **`rerun`** — how bento should construct the diagnostic-capture
  command. `kind: "append_args"` adds `args` to whatever `run` the
  user declared in `dish.toml` (the common case). `kind: "replace"`
  overrides the user's command outright (use when flags can't be
  safely appended).
- **`parser`** — either `"plugin"` (bento sends the captured output
  back to your plugin via `parseDiagnostics`) or one of the built-in
  parser ids (`"cargo-message"`, `"golangci-lint"`, `"eslint"`,
  `"ruff"`) when your tool happens to emit that format.

When the parser is a built-in, bento parses the output itself —
`parseDiagnostics` is never called for that task.

Hooks are strictly additive. If your plugin doesn't declare them,
failures still show the tool's stderr verbatim and bento just doesn't
populate `task.diagnostics` in the report.

## Error codes

Standard JSON-RPC 2.0 errors plus bento-specific additions:

| Code     | Meaning                       | `data` shape                       |
|----------|-------------------------------|------------------------------------|
| `-32700` | Parse error (malformed JSON)  | —                                  |
| `-32600` | Invalid request               | —                                  |
| `-32601` | Method not found              | —                                  |
| `-32602` | Invalid params                | —                                  |
| `-32603` | Internal error                | —                                  |
| `2001`   | `install` failed              | `{exit_status, command}`           |
| `2002`   | Required toolchain unparseable| `{file, reason}`                   |
| `2003`   | IO error inside plugin        | `{path, errno}`                    |
| `2099`   | Plugin internal error (catch-all) | `{detail}` — surfaced to user |

## Versioning

`protocol_version` is a single integer. We bump it on any **breaking**
change (method removal, required-field addition, semantic change).
Additive changes (new optional method, new optional capability) keep the
same version.

bento ships supporting *exactly one* protocol version per release. v0.x
breaks freely; post-1.0 we'll commit to N + N-1 acceptance and a
deprecation window. Plugins are expected to track bento releases — this
isn't a public ABI, it's a build tool's internal contract that happens
to cross a process boundary.

## A complete plugin in one file

The minimal Rust implementation is around 200 lines —
[`examples/bento-adapter-noop/src/main.rs`](../examples/bento-adapter-noop/src/main.rs).
Read it top-to-bottom; it covers framing, every method, and graceful
exit.

A minimal Python equivalent looks like this:

```python
#!/usr/bin/env python3
"""bento-adapter-noop in Python (~80 lines)."""
import json, os, sys, pathlib

PROTOCOL_VERSION = 1
ADAPTER_ID = "noop"

def read_msg():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line in (b"\r\n", b"\n"):
            break
        name, _, value = line.decode().partition(":")
        headers[name.strip().lower()] = value.strip()
    n = int(headers["content-length"])
    return json.loads(sys.stdin.buffer.read(n))

def write_msg(obj):
    body = json.dumps(obj).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode())
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

def respond(req_id, result):
    write_msg({"jsonrpc": "2.0", "id": req_id, "result": result})

def notify(method, params):
    write_msg({"jsonrpc": "2.0", "method": method, "params": params})

def main():
    while True:
        msg = read_msg()
        rid, method = msg.get("id"), msg.get("method", "")
        if method == "initialize":
            respond(rid, {
                "protocol_version": PROTOCOL_VERSION,
                "adapter_id": ADAPTER_ID,
                "display_name": "noop (Python)",
                "fingerprint_files": ["noop.toml"],
                "default_tasks": [{"name": "build", "run": "true"}],
                "capabilities": {"detect": True, "install": True,
                                 "required_toolchain": False,
                                 "resolved_toolchain_fingerprint": False},
            })
        elif method == "detect":
            d = msg["params"]["dir"]
            respond(rid, {"matches": pathlib.Path(d, "noop.toml").is_file()})
        elif method == "install":
            notify("notifications/log",
                   {"level": "info", "message": "noop install\n"})
            respond(rid, None)
        elif method == "shutdown":
            respond(rid, None)
            return
        elif rid is not None:
            write_msg({"jsonrpc": "2.0", "id": rid,
                       "error": {"code": -32601,
                                 "message": f"method not found: {method}"}})

if __name__ == "__main__":
    main()
```

Save as `bento-adapter-noop`, `chmod +x`, drop on `$PATH`. `bento plan` in
a workspace containing a dish with `language = "noop"` will pick it up.

For non-trivial plugins use a real JSON-RPC library
([`jsonrpc-2.0`](https://crates.io/crates/jsonrpc-2.0) on the Rust side,
[`jsonrpcserver`](https://www.jsonrpcserver.com/) on the Python side) —
the framing is what's bento-specific, the message shape is standard.

## Trust and "sandboxing"

**v1 trust model: plugins run with bento's full process privilege.** They
can read your home directory, exfiltrate env vars, `rm -rf` your
workspace. Same as any other binary you put on `$PATH`.

This is the right call: real sandboxing requires per-OS primitives
(landlock, seccomp, pledge) that don't ship anywhere usable on Windows.
Plugins are *deliberately installed* by the user — the threat model is no
different from `cargo install <crate>` or `brew install <formula>`.

If you want isolation, run your whole workspace in a container via the
existing `[execution] container = always` mode — plugins inherit that
boundary for free.

bento prints a one-line `loaded plugin: <id> from <path>` at verbose log
level so users can audit what got picked up.

## Known limits

**TTY for tool subprocesses.** When your plugin spawns a tool (say,
`rebar3 compile`), that tool sees a pipe, not a TTY. Progress bars and
ANSI colour will likely be suppressed. Plugins that care can spawn under
`script(1)` / a pty themselves; bento doesn't do this for you.

**Concurrency.** Calls to a single plugin are serialised — only one
in-flight request at a time per stdio channel. Different plugins are
queried in serial too in v1. Discovery against a workspace with 20
dishes × 2 plugins × sub-ms `detect` is tens of milliseconds — not
hot. If your plugin's queries are slow, profile and we'll talk about
pipelining.

**Cache key impact.** `adapter_id` participates in every cache key. If you
ship a breaking change to your plugin (different default tasks, new
fingerprint files, different `install` semantics), bump the adapter id
(`erlang` → `erlang2`) so existing caches don't poison subsequent runs.
This is the same convention an in-process rewrite would use.
