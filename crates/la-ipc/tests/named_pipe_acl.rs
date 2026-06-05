#![cfg(windows)]
//! WEK-81 — Windows Named Pipe ACL + peer-SID verification.
//!
//! Default-on tests run as the current user on every Windows CI runner and
//! exercise the round-trip + bind/accept invariants. The cross-user denial
//! case requires a second local user account and is gated by the
//! `LA_TEST_WINDOWS_CROSS_USER` env var — set it in dedicated cross-user
//! pipelines (alongside `LA_TEST_OTHER_USER` + `LA_TEST_OTHER_PASS`), leave
//! it unset in plain `cargo test`.

use la_ipc::transport::{self, Endpoint, Listener};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn unique_pipe_name(tag: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        r"\\.\pipe\la-ipc-acl-test-{tag}-{pid}-{nanos}",
        pid = std::process::id()
    )
}

/// Positive baseline. Same-user bind + connect + framed payload exchange.
/// Would catch a wrong-pointer / SD-size mistake in
/// `create_with_security_attributes_raw` since either an invalid SD or a
/// stale handle surfaces as an immediate IO error on either side.
#[tokio::test(flavor = "current_thread")]
async fn same_user_roundtrip() {
    let name = unique_pipe_name("rt");
    let listener = Listener::bind(&Endpoint::named_pipe(name.clone()))
        .await
        .expect("bind owner-only pipe");

    let server_task = tokio::spawn(async move {
        let mut s = listener.accept().await.expect("accept");
        let mut len_buf = [0u8; 4];
        s.read_exact(&mut len_buf).await.unwrap();
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len];
        s.read_exact(&mut body).await.unwrap();
        assert_eq!(&body, br#"{"jsonrpc":"2.0","method":"ping","id":1}"#);
        let pong = br#"{"jsonrpc":"2.0","result":"pong","id":1}"#;
        s.write_all(&(pong.len() as u32).to_be_bytes())
            .await
            .unwrap();
        s.write_all(pong).await.unwrap();
        s.shutdown().await.unwrap();
    });

    let mut c = transport::connect(&Endpoint::named_pipe(name))
        .await
        .expect("client connect");
    let req = br#"{"jsonrpc":"2.0","method":"ping","id":1}"#;
    c.write_all(&(req.len() as u32).to_be_bytes())
        .await
        .unwrap();
    c.write_all(req).await.unwrap();

    let mut len_buf = [0u8; 4];
    c.read_exact(&mut len_buf).await.unwrap();
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len];
    c.read_exact(&mut body).await.unwrap();
    assert_eq!(&body, br#"{"jsonrpc":"2.0","result":"pong","id":1}"#);

    server_task.await.unwrap();
}

/// Regression guard for the "second server instance silently falls back to
/// default DACL" bug that triggered WEK-81. After one accept(), the
/// pre-created NEXT instance must still be reachable by the same user (and
/// — once cross-user-denied below is enabled — must still reject other
/// users). We can't directly read the DACL through tokio's public API, so
/// this test walks the accept loop twice over the same `Listener` and
/// requires both client connects to succeed. Combined with
/// `cross_user_denied`, this covers both directions.
#[tokio::test(flavor = "current_thread")]
async fn second_instance_after_accept_still_locked() {
    let name = unique_pipe_name("seq");
    let listener = std::sync::Arc::new(
        Listener::bind(&Endpoint::named_pipe(name.clone()))
            .await
            .expect("bind"),
    );

    for round in 0..2u32 {
        let l = listener.clone();
        let server = tokio::spawn(async move {
            let mut s = l.accept().await.unwrap_or_else(|e| {
                panic!("round {round} accept failed: {e}");
            });
            let _ = s.shutdown().await;
        });
        let mut _c = transport::connect(&Endpoint::named_pipe(name.clone()))
            .await
            .unwrap_or_else(|e| panic!("round {round} connect failed: {e}"));
        let _ = _c.shutdown().await;
        server.await.unwrap();
    }
}

/// Lightweight wiring smoke: confirms that `Listener::bind` succeeds
/// (the ServerOptions chain — first_pipe_instance + reject_remote_clients
/// + owner-locked DACL — composes cleanly) and that dropping the listener
/// without ever accepting is safe (no SD leak / handle leak panic). Cheap,
/// no SMB needed, catches regressions where a future refactor drops one
/// of the three builder calls.
#[tokio::test(flavor = "current_thread")]
async fn bind_drop_roundtrip_is_clean() {
    let name = unique_pipe_name("rrc");
    let listener = Listener::bind(&Endpoint::named_pipe(name)).await.unwrap();
    drop(listener);
}

/// Cross-user denial — the actual contract WEK-81 promises. Gated behind
/// `LA_TEST_WINDOWS_CROSS_USER` because GitHub-hosted Windows runners
/// don't provision a second local user by default. A dedicated
/// cross-user CI job (or a developer with `runas /savecred` set up) flips
/// the env trio (LA_TEST_WINDOWS_CROSS_USER + LA_TEST_OTHER_USER +
/// LA_TEST_OTHER_PASS) to enable it.
#[tokio::test(flavor = "current_thread")]
async fn cross_user_denied() {
    if std::env::var("LA_TEST_WINDOWS_CROSS_USER").is_err() {
        eprintln!("skip: LA_TEST_WINDOWS_CROSS_USER not set");
        return;
    }
    let other_user = std::env::var("LA_TEST_OTHER_USER")
        .expect("LA_TEST_OTHER_USER required when LA_TEST_WINDOWS_CROSS_USER is set");
    let _other_pass = std::env::var("LA_TEST_OTHER_PASS")
        .expect("LA_TEST_OTHER_PASS required when LA_TEST_WINDOWS_CROSS_USER is set");

    let name = unique_pipe_name("cross");
    let _listener = Listener::bind(&Endpoint::named_pipe(name.clone()))
        .await
        .expect("bind");

    // PowerShell helper: open the pipe as `other_user` via `runas
    // /savecred` and assert the open returns UnauthorizedAccessException
    // (ERROR_ACCESS_DENIED, exit code 5). `runas` reads the stored
    // credential for `other_user`; the LA_TEST_OTHER_PASS env var is
    // documented but not piped here — Windows `runas` has no stdin pwd
    // intake. CI must `cmdkey /add` the cred ahead of time.
    let pipe_short = name.trim_start_matches(r"\\.\pipe\");
    let ps = format!(
        "$p = New-Object System.IO.Pipes.NamedPipeClientStream('.','{pipe}',[System.IO.Pipes.PipeDirection]::InOut);\
         try {{ $p.Connect(2000); exit 0 }}\
         catch [System.UnauthorizedAccessException] {{ exit 5 }}\
         catch {{ exit 1 }}",
        pipe = pipe_short
    );

    let status = std::process::Command::new("runas")
        .args([
            &format!("/user:{other_user}"),
            "/savecred",
            &format!("powershell -NoProfile -Command \"{ps}\""),
        ])
        .status()
        .expect("spawn runas");

    assert_eq!(
        status.code(),
        Some(5),
        "expected ACCESS_DENIED (exit 5) on cross-user open"
    );
}
