//! End-to-end test for the `la-ipc-cat` debug shim.
//!
//! Validates the README claim that you can pipe JSON-RPC frames in via stdin
//! and get response frames out via stdout. We spin up a mock daemon that
//! answers `initialize`, then spawn the cargo-built `la-ipc-cat` binary
//! with `--endpoint uds:<path>` and feed it one line.
//!
//! Linux-only (UDS); the test is gated to avoid pretending to validate
//! macOS/Windows per the WEK-5 scope decision.

#![cfg(unix)]

use std::io::Write;
use std::process::Stdio;
use std::time::Duration;

use la_ipc::transport::{Endpoint, Listener};
use la_ipc::Connection;
use la_proto::jsonrpc::{Message, Response, ResponseOutcome};
use la_proto::methods::{Initialize, InitializeResult, Method, ServerCapabilities};
use la_proto::PROTOCOL_VERSION;
use tempfile::TempDir;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn la_ipc_cat_round_trips_an_initialize_request() {
    let dir = TempDir::new().unwrap();
    let sock = dir.path().join("lad.sock");

    let listener = Listener::bind(&Endpoint::uds(&sock)).await.unwrap();
    let sock_for_child = sock.clone();

    // Simple mock daemon: accept one connection, respond to one request.
    let daemon = tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
        let mut conn = Connection::new(stream);
        let msg = conn.recv().await.unwrap().unwrap();
        let Message::Request(req) = msg else {
            panic!("not a request")
        };
        assert_eq!(req.method, Initialize::NAME);
        let result = InitializeResult {
            server: "lad".into(),
            server_version: "test".into(),
            protocol_version: PROTOCOL_VERSION.into(),
            capabilities: ServerCapabilities::default(),
        };
        let resp = Response::success(req.id, &result).unwrap();
        conn.send(&Message::Response(resp)).await.unwrap();
    });

    // Locate the cargo-built binary. CARGO_BIN_EXE_<name> is set by Cargo
    // for any binary in this crate, but only for unit tests inside the crate
    // — for integration tests we have to ask cargo where it built it.
    let bin = env!("CARGO_BIN_EXE_la-ipc-cat");

    let req_line = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"client":"la","client_version":"0.1","protocol_versions":["{PROTOCOL_VERSION}"]}}}}"#
    );

    // Spawn the binary, feed stdin, read stdout, give it a short timeout.
    let mut child = std::process::Command::new(bin)
        .args(["--endpoint", &format!("uds:{}", sock_for_child.display())])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn la-ipc-cat");

    // Write one line + close stdin.
    {
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, "{req_line}").unwrap();
    }

    // Read all of stdout, with timeout.
    let out = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap());
    let output = tokio::time::timeout(Duration::from_secs(5), out)
        .await
        .expect("la-ipc-cat timed out")
        .unwrap();
    assert!(
        output.status.success(),
        "la-ipc-cat exited non-zero; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    // Should be a single JSON object on its own line.
    let line = stdout.lines().next().expect("no stdout line emitted");
    let v: serde_json::Value = serde_json::from_str(line).expect("stdout not valid JSON");
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 1);
    assert_eq!(v["result"]["protocol_version"], PROTOCOL_VERSION);

    // Also confirm the response decodes through our high-level decoder.
    let msg = Message::from_slice(line.as_bytes()).expect("Message decode");
    let Message::Response(Response {
        outcome: ResponseOutcome::Result(result),
        ..
    }) = msg
    else {
        panic!("not a result response");
    };
    let info: InitializeResult = serde_json::from_value(result).unwrap();
    assert_eq!(info.protocol_version, PROTOCOL_VERSION);

    daemon.await.unwrap();
}
