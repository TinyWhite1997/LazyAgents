//! `mock-cli` — a deterministic stand-in for `claude` used by
//! `la-adapter` tests. Lives under `tests/bin/` so it ships as a
//! crate-internal binary that integration tests can locate via the
//! Cargo `CARGO_BIN_EXE_<name>` env var.
//!
//! Supported invocations (only those the adapter tests need today):
//!
//! ```text
//! mock-cli --version                # prints "2.1.158 (Claude Code)\n", exits 0
//! mock-cli --version --mode unauth  # prints unauth message to stderr, exits 1
//! mock-cli --version --mode missing # exits 127 with empty output
//! mock-cli --version --mode garbage # prints unrelated text, exits 0
//! mock-cli --print <prompt>         # echoes the prompt, exits 0
//! ```
//!
//! Mode can also be supplied via env var `MOCK_CLI_MODE=ok|unauth|garbage`.

use std::env;
use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let mode = pick_mode(&args).unwrap_or_else(|| {
        env::var("MOCK_CLI_MODE").unwrap_or_else(|_| "ok".into())
    });

    let subcmd = args.iter().find(|a| !a.starts_with("--mode")).cloned();

    match subcmd.as_deref() {
        Some("--version") => version(&mode),
        Some("--print") => print_prompt(&args),
        _ => {
            let _ = writeln!(std::io::stderr(), "mock-cli: unknown invocation: {:?}", args);
            ExitCode::from(2)
        }
    }
}

fn pick_mode(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(a) = iter.next() {
        if a == "--mode" {
            return iter.next().cloned();
        }
        if let Some(rest) = a.strip_prefix("--mode=") {
            return Some(rest.to_string());
        }
    }
    None
}

fn version(mode: &str) -> ExitCode {
    match mode {
        "ok" => {
            println!("2.1.158 (Claude Code)");
            ExitCode::SUCCESS
        }
        "unauth" => {
            let _ = writeln!(
                std::io::stderr(),
                "Error: not logged in. Run `claude login` to authenticate."
            );
            ExitCode::from(1)
        }
        "garbage" => {
            println!("welcome to nothing in particular");
            ExitCode::SUCCESS
        }
        other => {
            let _ = writeln!(std::io::stderr(), "mock-cli: unknown mode: {other}");
            ExitCode::from(3)
        }
    }
}

fn print_prompt(args: &[String]) -> ExitCode {
    let prompt = args
        .iter()
        .skip_while(|a| *a != "--print")
        .nth(1)
        .cloned()
        .unwrap_or_default();
    println!("MOCK_REPLY: {prompt}");
    ExitCode::SUCCESS
}
