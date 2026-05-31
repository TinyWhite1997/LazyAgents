//! `la-ipc-cat` — debug shim for poking the daemon socket from the shell.
//!
//! Reads JSON-RPC messages from stdin (one JSON object per line, blank lines
//! ignored), frames them with the 4-byte BE length prefix, sends them to
//! the daemon, and writes incoming frames to stdout (one JSON object per line
//! so it composes with `jq`). Designed to be piped:
//!
//! ```bash
//! cat requests.jsonl | la-ipc-cat --endpoint uds:/run/lazyagents/lad.sock | jq .
//! ```
//!
//! The first line MUST be an `initialize` request because the daemon will
//! reject anything else; we don't auto-handshake on purpose, so users can
//! exercise the handshake itself.
//!
//! ## Endpoint syntax
//! - `uds:/path/to/socket` (Unix)
//! - `pipe:\\.\pipe\name` (Windows)
//! - bare path → UDS on Unix, Named Pipe on Windows
//!
//! ## Idle timeout
//!
//! After stdin EOF the binary keeps reading server frames until the server
//! closes its side OR the idle timeout elapses. Tunable via
//! `--idle-timeout <secs>` (default 5 s); pass `0` to wait indefinitely.

use std::process::ExitCode;
use std::time::Duration;

use bytes::Bytes;
use futures_util::SinkExt;
use futures_util::StreamExt;
use la_ipc::codec::FrameCodec;
use la_ipc::transport::{connect, Endpoint};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::codec::Framed;

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let cfg = match parse_args(&args) {
        Ok(c) => c,
        Err(usage) => {
            eprintln!("{usage}");
            return ExitCode::from(2);
        }
    };

    let stream = match connect(&cfg.endpoint).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("la-ipc-cat: connect {:?}: {e}", cfg.endpoint);
            return ExitCode::from(1);
        }
    };
    let framed = Framed::new(stream, FrameCodec::new());
    let (mut sink, mut src) = framed.split();

    // Forward incoming frames to stdout, one line each.
    let recv_task = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        let mut frames = 0u64;
        while let Some(frame) = src.next().await {
            match frame {
                Ok(bytes) => {
                    if stdout.write_all(&bytes).await.is_err() {
                        break;
                    }
                    if stdout.write_all(b"\n").await.is_err() {
                        break;
                    }
                    let _ = stdout.flush().await;
                    frames += 1;
                }
                Err(e) => {
                    eprintln!("la-ipc-cat: recv error: {e}");
                    break;
                }
            }
        }
        frames
    });

    // Pump stdin lines as outbound frames.
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    let mut sent: u64 = 0;
    let mut send_exit: Option<ExitCode> = None;
    loop {
        match stdin.next_line().await {
            Ok(Some(line)) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if let Err(e) = sink.send(Bytes::copy_from_slice(trimmed.as_bytes())).await {
                    eprintln!("la-ipc-cat: send error: {e}");
                    send_exit = Some(ExitCode::from(1));
                    break;
                }
                sent += 1;
            }
            Ok(None) => break,
            Err(e) => {
                eprintln!("la-ipc-cat: stdin read error: {e}");
                send_exit = Some(ExitCode::from(1));
                break;
            }
        }
    }

    // Half-close write side so the daemon sees a clean EOF and stops
    // waiting for more requests; this also lets `recv_task` drain remaining
    // notifications without the daemon holding both halves open.
    let _ = sink.close().await;

    // If we never sent a frame, surface that (the doc says first line MUST
    // be initialize, so a script that fed an empty stdin almost certainly
    // had a bug).
    if sent == 0 {
        eprintln!("la-ipc-cat: no frames read from stdin; expected at least an initialize request");
        return ExitCode::from(1);
    }

    // Wait for the recv side to drain. `idle_timeout_secs == 0` ⇒ wait
    // forever; otherwise enforce the configured ceiling so a hung daemon
    // doesn't pin this process forever.
    let frames_back = if cfg.idle_timeout_secs == 0 {
        recv_task.await.unwrap_or(0)
    } else {
        match tokio::time::timeout(Duration::from_secs(cfg.idle_timeout_secs), recv_task).await {
            Ok(res) => res.unwrap_or(0),
            Err(_) => {
                eprintln!(
                    "la-ipc-cat: idle timeout after {}s with response still pending",
                    cfg.idle_timeout_secs
                );
                return ExitCode::from(3);
            }
        }
    };
    let _ = frames_back; // currently informational

    send_exit.unwrap_or(ExitCode::SUCCESS)
}

struct Cfg {
    endpoint: Endpoint,
    /// 0 = wait forever; otherwise max seconds after stdin EOF to keep
    /// draining server responses.
    idle_timeout_secs: u64,
}

fn parse_args(args: &[String]) -> Result<Cfg, String> {
    // Tiny ad-hoc parser; pulling in clap for a debug shim isn't worth it.
    let mut i = 1;
    let mut endpoint: Option<String> = None;
    let mut idle_timeout_secs: u64 = 5;
    while i < args.len() {
        match args[i].as_str() {
            "--endpoint" | "-e" => {
                i += 1;
                endpoint = args.get(i).cloned();
            }
            "--idle-timeout" => {
                i += 1;
                idle_timeout_secs = args
                    .get(i)
                    .ok_or_else(|| "missing --idle-timeout value".to_string())?
                    .parse()
                    .map_err(|e| format!("--idle-timeout: {e}"))?;
            }
            "-h" | "--help" => return Err(usage()),
            other => return Err(format!("unknown argument: {other}\n\n{}", usage())),
        }
        i += 1;
    }
    let raw = endpoint.ok_or_else(|| format!("missing --endpoint\n\n{}", usage()))?;
    Ok(Cfg {
        endpoint: parse_endpoint(&raw)?,
        idle_timeout_secs,
    })
}

fn parse_endpoint(s: &str) -> Result<Endpoint, String> {
    if let Some(rest) = s.strip_prefix("uds:") {
        Ok(Endpoint::uds(rest))
    } else if let Some(rest) = s.strip_prefix("pipe:") {
        Ok(Endpoint::named_pipe(rest))
    } else if cfg!(windows) {
        Ok(Endpoint::named_pipe(s))
    } else {
        Ok(Endpoint::uds(s))
    }
}

fn usage() -> String {
    "la-ipc-cat — pipe JSON-RPC frames to/from a LazyAgents daemon.\n\
     \n\
     Usage: la-ipc-cat --endpoint <uds:/path | pipe:\\\\.\\pipe\\name | path>\n\
     \n\
     Options:\n\
       -e, --endpoint <ep>         Required. UDS path or Named Pipe name.\n\
           --idle-timeout <secs>   Wait at most N seconds for server frames\n\
                                   after stdin EOF (default 5; 0 = forever).\n\
       -h, --help                  Show this help.\n\
     \n\
     Reads one JSON object per line from stdin and writes one JSON object\n\
     per line to stdout. The first line MUST be an initialize request."
        .into()
}
