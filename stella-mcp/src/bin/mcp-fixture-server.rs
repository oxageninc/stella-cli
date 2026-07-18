//! A tiny, self-contained MCP server for the `stella-mcp` integration tests.
//! It speaks newline-delimited JSON-RPC on stdin/stdout with canned answers to
//! `initialize`, `tools/list`, and `tools/call`, plus fault-injection flags
//! for the resilience tests. It is **not** part of the shipping surface — only
//! `tests/` build and launch it (via `env!("CARGO_BIN_EXE_mcp-fixture-server")`).
//!
//! Flags:
//! - `--protocol-version <v>`: counter-offer `<v>` in the initialize result.
//! - `--paginate`: return `tools/list` across two cursor pages.
//! - `--hang`: never answer `tools/call` (exercises the call timeout).
//! - `--die-after <n>`: exit(0) upon the `(n+1)`-th request, before answering
//!   it (exercises mid-call server death).
//! - `--garbage`: answer `tools/call` with an unparseable `result`.
//!
//! Tools it advertises: `echo`, `env_probe`, `make_image`, `make_resource`,
//! `fail` (`isError:true`), `jsonrpc_error` (a JSON-RPC error response).

use std::io::{self, BufRead, Write};
use std::time::Duration;

use serde_json::{Value, json};

struct Flags {
    hang: bool,
    die_after: Option<u64>,
    garbage: bool,
    paginate: bool,
    protocol_version: String,
}

impl Flags {
    fn parse() -> Self {
        let mut flags = Flags {
            hang: false,
            die_after: None,
            garbage: false,
            paginate: false,
            protocol_version: "2025-06-18".to_string(),
        };
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--hang" => flags.hang = true,
                "--garbage" => flags.garbage = true,
                "--paginate" => flags.paginate = true,
                "--die-after" => {
                    i += 1;
                    if i < args.len() {
                        flags.die_after = args[i].parse().ok();
                    }
                }
                "--protocol-version" => {
                    i += 1;
                    if i < args.len() {
                        flags.protocol_version = args[i].clone();
                    }
                }
                _ => {}
            }
            i += 1;
        }
        flags
    }
}

fn main() {
    let flags = Flags::parse();
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut served: u64 = 0;

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => continue,
        };

        // Notifications (no id) get no response.
        let id = match message.get("id") {
            Some(id) if !id.is_null() => id.clone(),
            _ => continue,
        };

        // `--die-after`: exit before answering once we've served enough.
        if let Some(limit) = flags.die_after
            && served >= limit
        {
            std::process::exit(0);
        }

        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);

        let response = match method {
            "initialize" => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": flags.protocol_version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "mcp-fixture-server", "version": "0.1.0" }
                }
            }),
            "tools/list" => tools_list_response(&id, &params, &flags),
            "tools/call" => {
                if flags.hang {
                    loop {
                        std::thread::sleep(Duration::from_secs(3600));
                    }
                }
                tools_call_response(&id, &params, &flags)
            }
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": "method not found" }
            }),
        };

        write_line(&mut stdout, &response);
        served += 1;
    }
}

fn write_line(stdout: &mut io::Stdout, value: &Value) {
    let mut line = serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string());
    line.push('\n');
    let _ = stdout.write_all(line.as_bytes());
    let _ = stdout.flush();
}

fn tool_def(name: &str) -> Value {
    json!({
        "name": name,
        "description": format!("fixture tool {name}"),
        "inputSchema": { "type": "object" }
    })
}

fn all_tools() -> Vec<Value> {
    [
        "echo",
        "env_probe",
        "make_image",
        "make_resource",
        "fail",
        "jsonrpc_error",
    ]
    .iter()
    .map(|name| tool_def(name))
    .collect()
}

fn tools_list_response(id: &Value, params: &Value, flags: &Flags) -> Value {
    if flags.paginate {
        let cursor = params.get("cursor").and_then(Value::as_str);
        return match cursor {
            None => json!({
                "jsonrpc": "2.0", "id": id,
                "result": { "tools": [tool_def("alpha")], "nextCursor": "page2" }
            }),
            Some("page2") => json!({
                "jsonrpc": "2.0", "id": id,
                "result": { "tools": [tool_def("beta")] }
            }),
            Some(_) => json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": [] } }),
        };
    }
    json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": all_tools() } })
}

fn tools_call_response(id: &Value, params: &Value, flags: &Flags) -> Value {
    // `--garbage`: a well-formed JSON-RPC envelope whose `result` is not a
    // tool-call result — the client must fail to decode it, not crash.
    if flags.garbage {
        return json!({ "jsonrpc": "2.0", "id": id, "result": 42 });
    }

    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
    let tool = params.get("name").and_then(Value::as_str).unwrap_or("");

    let result = match tool {
        "echo" => json!({
            "content": [{ "type": "text", "text": format!("echo: {arguments}") }],
            "isError": false
        }),
        "env_probe" => {
            let var = arguments.get("var").and_then(Value::as_str).unwrap_or("");
            let value = std::env::var(var).unwrap_or_else(|_| "unset".to_string());
            json!({ "content": [{ "type": "text", "text": value }] })
        }
        "make_image" => json!({
            "content": [{ "type": "image", "data": "AAAA", "mimeType": "image/png" }]
        }),
        "make_resource" => json!({
            "content": [{ "type": "resource", "resource": { "uri": "file:///r.txt", "text": "hi" } }]
        }),
        "fail" => json!({
            "content": [{ "type": "text", "text": "the tool failed" }],
            "isError": true
        }),
        "jsonrpc_error" => {
            return json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32000, "message": "boom from server" }
            });
        }
        other => json!({
            "content": [{ "type": "text", "text": format!("unknown fixture tool: {other}") }],
            "isError": true
        }),
    };

    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}
