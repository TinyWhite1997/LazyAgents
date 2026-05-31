use std::time::Duration;

use la_ipc::{FramedJson, RpcRequest, RpcResponse};
use la_pty::{spawn, CommandBuilder, PtyChild, PtySize, Signal};
use serde_json::{json, Value};
use tokio::time::timeout;

const READ_TIMEOUT: Duration = Duration::from_secs(15);
const SESSION_ID: &str = "m0-session-1";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m0_attach_write_reply_detach_keeps_pty_alive() {
    let (client_stream, daemon_stream) = tokio::io::duplex(64 * 1024);
    let daemon = tokio::spawn(async move { run_mock_daemon(daemon_stream).await });
    let mut client = FramedJson::new(client_stream);

    let create = request(
        &mut client,
        1,
        "sessions.create",
        json!({ "backend": "claude" }),
    )
    .await;
    assert_eq!(create["session_id"], SESSION_ID);
    assert!(
        create["initial_output"]
            .as_str()
            .unwrap_or_default()
            .contains("mock-claude ready"),
        "mock claude should announce readiness: {create:?}"
    );

    let attach = request(
        &mut client,
        2,
        "sessions.attach",
        json!({ "session_id": SESSION_ID, "replay_bytes": 0 }),
    )
    .await;
    assert_eq!(attach["snapshot_seq"], 0);

    let write = request(
        &mut client,
        3,
        "sessions.write",
        json!({ "session_id": SESSION_ID, "bytes": "hello m0" }),
    )
    .await;
    assert!(
        write["output"]
            .as_str()
            .unwrap_or_default()
            .contains("mock-claude reply: hello m0"),
        "expected mock claude reply after PTY write: {write:?}"
    );

    let detach = request(
        &mut client,
        4,
        "sessions.detach",
        json!({ "session_id": SESSION_ID }),
    )
    .await;
    assert_eq!(detach, json!({}));

    let probe = request(
        &mut client,
        5,
        "sessions.probe_alive",
        json!({ "session_id": SESSION_ID }),
    )
    .await;
    assert_eq!(probe["running"], true);
    assert!(
        probe["output"]
            .as_str()
            .unwrap_or_default()
            .contains("mock-claude reply: post-detach"),
        "detach must not kill the PTY child: {probe:?}"
    );

    let shutdown = request(&mut client, 6, "daemon.shutdown", json!({})).await;
    assert_eq!(shutdown, json!({ "shutdown": true }));
    daemon.await.expect("daemon task").expect("daemon result");
}

async fn request(
    client: &mut FramedJson<tokio::io::DuplexStream>,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    client
        .write_json(&RpcRequest::new(id, method, params))
        .await
        .expect("send request");
    let response: RpcResponse = client.read_json().await.expect("read response");
    assert_eq!(response.id, id);
    if let Some(error) = response.error {
        panic!("RPC {method} failed: {} {}", error.code, error.message);
    }
    response.result.expect("result")
}

async fn run_mock_daemon(
    stream: tokio::io::DuplexStream,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut ipc = FramedJson::new(stream);
    let mut child: Option<PtyChild> = None;
    let mut attached = false;

    loop {
        let request: RpcRequest = ipc.read_json().await?;
        let response = match request.method.as_str() {
            "sessions.create" => {
                if child.is_some() {
                    RpcResponse::error(request.id, -32000, "session already exists")
                } else {
                    let mut spawned = spawn(mock_claude_command(), PtySize::default())?;
                    let initial = read_until(&mut spawned, "mock-claude ready").await?;
                    child = Some(spawned);
                    RpcResponse::result(
                        request.id,
                        json!({ "session_id": SESSION_ID, "backend": "claude", "initial_output": initial }),
                    )
                }
            }
            "sessions.attach" => {
                if child.is_none() {
                    RpcResponse::error(request.id, -32001, "no session")
                } else {
                    attached = true;
                    RpcResponse::result(request.id, json!({ "snapshot_seq": 0 }))
                }
            }
            "sessions.write" => {
                if !attached {
                    RpcResponse::error(request.id, -32002, "session is not attached")
                } else if let Some(session) = child.as_mut() {
                    let bytes = request.params["bytes"].as_str().unwrap_or_default();
                    write_mock_prompt(session, bytes).await?;
                    let output =
                        read_until(session, &format!("mock-claude reply: {bytes}")).await?;
                    RpcResponse::result(request.id, json!({ "output": output }))
                } else {
                    RpcResponse::error(request.id, -32001, "no session")
                }
            }
            "sessions.detach" => {
                attached = false;
                RpcResponse::result(request.id, json!({}))
            }
            "sessions.probe_alive" => {
                if let Some(session) = child.as_mut() {
                    write_mock_prompt(session, "post-detach").await?;
                    let output = read_until(session, "mock-claude reply: post-detach").await?;
                    RpcResponse::result(request.id, json!({ "running": true, "output": output }))
                } else {
                    RpcResponse::result(request.id, json!({ "running": false }))
                }
            }
            "daemon.shutdown" => {
                if let Some(session) = child.take() {
                    let _ = session.signal(Signal::Kill);
                    let _ = timeout(Duration::from_secs(5), session.wait()).await;
                }
                ipc.write_json(&RpcResponse::result(
                    request.id,
                    json!({ "shutdown": true }),
                ))
                .await?;
                break;
            }
            _ => RpcResponse::error(
                request.id,
                -32601,
                format!("unknown method {}", request.method),
            ),
        };
        ipc.write_json(&response).await?;
    }

    Ok(())
}

async fn write_mock_prompt(
    child: &mut PtyChild,
    prompt: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if cfg!(windows) {
        child
            .writer
            .write(format!("echo mock-claude reply: {prompt}\r\n"))
            .await?;
    } else {
        child.writer.write(format!("{prompt}\n")).await?;
    }
    Ok(())
}

async fn read_until(
    child: &mut PtyChild,
    needle: &str,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let mut buf = Vec::new();
    timeout(READ_TIMEOUT, async {
        while let Some(chunk) = child.reader.recv().await {
            buf.extend_from_slice(&chunk);
            let text = String::from_utf8_lossy(&buf);
            if text.contains(needle) {
                return Ok(text.into_owned());
            }
        }
        Err(format!("PTY closed before seeing {needle:?}").into())
    })
    .await
    .map_err(|_| {
        format!(
            "timed out waiting for {needle:?}; buffered {}",
            String::from_utf8_lossy(&buf)
        )
    })?
}

fn mock_claude_command() -> CommandBuilder {
    if cfg!(windows) {
        let mut cmd = CommandBuilder::new("cmd.exe");
        cmd.args(["/Q", "/K", "echo mock-claude ready"]);
        cmd
    } else {
        let mut cmd = CommandBuilder::new("sh");
        cmd.args([
            "-c",
            "printf 'mock-claude ready\\n'; while IFS= read -r line; do printf 'mock-claude reply: %s\\n' \"$line\"; done",
        ]);
        cmd
    }
}
