//! Server-side stdio framing regression tests for `claw mcp serve`.
//!
//! `McpServer::run` reads/writes real stdin/stdout, so it cannot be unit-tested
//! with injected streams. These subprocess tests pin the spec-compliant NDJSON
//! framing (newline-delimited JSON, never LSP `Content-Length` headers) that the
//! transport switched to so claw interoperates with real MCP SDKs like serena.

use std::io::{Read, Write};
use std::process::{Command, Stdio};

fn run_mcp_serve(stdin_bytes: &[u8]) -> String {
    let mut child = Command::new(env!("CARGO_BIN_EXE_claw"))
        .args(["mcp", "serve"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn claw mcp serve");

    child
        .stdin
        .take()
        .expect("child stdin")
        .write_all(stdin_bytes)
        .expect("write request");
    // Dropping stdin signals EOF; the serve loop returns on clean EOF.

    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("child stdout")
        .read_to_string(&mut stdout)
        .expect("read response");

    child.wait().expect("wait for child");
    stdout
}

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"smoke","version":"0.0.1"}}}"#;

#[test]
fn mcp_serve_responds_with_ndjson_and_no_content_length_header() {
    let stdout = run_mcp_serve(format!("{INITIALIZE}\n").as_bytes());

    assert!(
        !stdout.to_ascii_lowercase().contains("content-length"),
        "stdio response must not use LSP Content-Length framing: {stdout:?}"
    );

    let lines: Vec<&str> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "expected exactly one NDJSON line: {stdout:?}"
    );

    let response: serde_json::Value =
        serde_json::from_str(lines[0]).expect("response line must parse as JSON");
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["serverInfo"]["name"], "claw");
    assert!(response["error"].is_null());
}

#[test]
fn mcp_serve_skips_leading_blank_lines_before_a_message() {
    // Blank/whitespace-only separator lines must be tolerated, not treated as
    // EOF or as a protocol error.
    let stdout = run_mcp_serve(format!("\n\r\n   \n{INITIALIZE}\n").as_bytes());

    let lines: Vec<&str> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "expected exactly one NDJSON line: {stdout:?}"
    );

    let response: serde_json::Value =
        serde_json::from_str(lines[0]).expect("response line must parse as JSON");
    assert_eq!(response["id"], 1);
    assert!(response["result"].is_object());
}

#[test]
fn mcp_serve_fails_fast_on_non_json_line_instead_of_skipping_it() {
    // A non-blank, non-JSON line is a protocol violation: it must surface as a
    // JSON-RPC parse error (-32700), never be silently skipped (which would let
    // a misbehaving peer stall the session).
    let stdout = run_mcp_serve(b"this is not json\n");

    let lines: Vec<&str> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 1, "expected one parse-error line: {stdout:?}");

    let response: serde_json::Value =
        serde_json::from_str(lines[0]).expect("error response must itself be NDJSON");
    assert_eq!(response["error"]["code"], -32700);
}
