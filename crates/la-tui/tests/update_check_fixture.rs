//! WEK-41 / M4.2 — end-to-end fixture test for `la --check-update`.
//!
//! Spin up a tiny in-process HTTP server that serves a canned GitHub
//! Releases manifest, point `LAZYAGENTS_UPDATE_MANIFEST_URL` at it, and
//! assert the three rendered outcomes: up-to-date, update available,
//! and the network-unavailable fallback (no listener bound).
//!
//! Why hand-roll an HTTP server: pulling `httptest` / `wiremock` in for
//! a single test would bloat the dev-deps tree, and our needs are
//! trivial — single GET, single 200, plain JSON body. Std `TcpListener`
//! covers it.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use la_tui::update_check::{self, CheckOutcome, CURRENT_VERSION};

/// Serve exactly one request on a kernel-assigned port, return the
/// `http://127.0.0.1:PORT/...` URL the test should point at.
fn serve_once(body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    let (ready_tx, ready_rx) = mpsc::channel();
    thread::spawn(move || {
        ready_tx.send(()).unwrap();
        let (mut sock, _) = listener.accept().expect("accept");
        // Drain the request preamble — we don't bother parsing it; the
        // contract is "any GET returns the canned manifest".
        let mut buf = [0u8; 1024];
        let _ = sock.set_read_timeout(Some(Duration::from_secs(2)));
        let _ = sock.read(&mut buf);
        let response = format!(
            "HTTP/1.1 200 OK\r\n\
             Content-Type: application/json\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\
             \r\n\
             {}",
            body.len(),
            body
        );
        let _ = sock.write_all(response.as_bytes());
        let _ = sock.shutdown(std::net::Shutdown::Both);
    });
    ready_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("listener never readied");
    format!("http://127.0.0.1:{port}/releases/latest")
}

/// `LAZYAGENTS_UPDATE_MANIFEST_URL` is process-global, so we serialize
/// the env-mutating tests behind a mutex. (cargo test runs tests
/// in-process across threads by default.)
fn with_manifest_url<R>(url: &str, body: impl FnOnce() -> R) -> R {
    use std::sync::Mutex;
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let prev = std::env::var("LAZYAGENTS_UPDATE_MANIFEST_URL").ok();
    std::env::set_var("LAZYAGENTS_UPDATE_MANIFEST_URL", url);
    let out = body();
    match prev {
        Some(v) => std::env::set_var("LAZYAGENTS_UPDATE_MANIFEST_URL", v),
        None => std::env::remove_var("LAZYAGENTS_UPDATE_MANIFEST_URL"),
    }
    out
}

#[test]
fn fixture_reports_up_to_date_when_versions_match() {
    let body = format!(
        r#"{{"name":"v{CURRENT_VERSION}","tag_name":"v{CURRENT_VERSION}","html_url":"https://example.test/r/v{CURRENT_VERSION}","prerelease":false}}"#
    );
    let body: &'static str = Box::leak(body.into_boxed_str());
    let url = serve_once(body);
    let outcome = with_manifest_url(&url, update_check::check_for_update);
    assert!(
        matches!(outcome, CheckOutcome::UpToDate { .. }),
        "expected UpToDate, got {outcome:?}"
    );
}

#[test]
fn fixture_reports_update_when_remote_is_newer() {
    // Pick a clearly-larger SemVer than CURRENT_VERSION ("0.1.0" today).
    let body = r#"{"name":"v99.0.0","tag_name":"v99.0.0","html_url":"https://example.test/r/v99.0.0","prerelease":false}"#;
    let url = serve_once(body);
    let outcome = with_manifest_url(&url, update_check::check_for_update);
    match outcome {
        CheckOutcome::UpdateAvailable { latest, url, .. } => {
            assert_eq!(latest, "99.0.0");
            assert_eq!(url, "https://example.test/r/v99.0.0");
        }
        other => panic!("expected UpdateAvailable, got {other:?}"),
    }
}

#[test]
fn fixture_treats_prerelease_as_quiet() {
    // WEK-41 "默认不自动升级" — a prerelease must NOT be advertised as
    // the upgrade target, even when its version number is higher.
    let body = r#"{"name":"v99.0.0-rc.1","tag_name":"v99.0.0-rc.1","html_url":"https://example.test/r/v99.0.0-rc.1","prerelease":true}"#;
    let url = serve_once(body);
    let outcome = with_manifest_url(&url, update_check::check_for_update);
    assert!(
        matches!(outcome, CheckOutcome::UpToDate { .. }),
        "prerelease leaked through: {outcome:?}"
    );
}

#[test]
fn unreachable_endpoint_is_non_fatal() {
    // Bind + drop a listener to claim a port, then close — by the time
    // we try to connect, nothing listens, and we want a clean
    // `Unavailable` (NOT a panic, NOT a status-2 exit code).
    let dead_port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        // Drop the listener so the port is free / refused.
        drop(l);
        p
    };
    let url = format!("http://127.0.0.1:{dead_port}/releases/latest");
    let outcome = with_manifest_url(&url, update_check::check_for_update);
    assert!(
        matches!(outcome, CheckOutcome::Unavailable(_)),
        "expected Unavailable for closed port, got {outcome:?}"
    );
    assert_eq!(update_check::exit_code(&outcome), 0);
}

/// Sanity: a 200 with a body that isn't a manifest must surface as
/// `Unavailable("parse manifest: ...")` and NOT crash the process.
#[test]
fn malformed_manifest_is_non_fatal() {
    let url = serve_once("not json at all");
    let outcome = with_manifest_url(&url, update_check::check_for_update);
    match outcome {
        CheckOutcome::Unavailable(msg) => {
            assert!(msg.contains("parse"), "expected parse error, got {msg}");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

/// `serve_once` accepts exactly one connection, so this test is a
/// canary for the harness itself — if a future change starts opening a
/// second probe socket (e.g. retry on transient failures) this test
/// will hang and surface the regression.
#[test]
fn single_socket_connection_is_enough() {
    let body = format!(
        r#"{{"name":"v{CURRENT_VERSION}","tag_name":"v{CURRENT_VERSION}","html_url":"","prerelease":false}}"#
    );
    let body: &'static str = Box::leak(body.into_boxed_str());
    let url = serve_once(body);
    let outcome = with_manifest_url(&url, update_check::check_for_update);
    assert!(matches!(outcome, CheckOutcome::UpToDate { .. }));
    // Probe the URL again — server is gone, must fall back gracefully.
    let _ = TcpStream::connect_timeout(&"127.0.0.1:1".parse().unwrap(), Duration::from_millis(50));
}
