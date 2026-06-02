//! `mock-cli` — a deterministic stand-in for `claude` and `codex` used
//! by `la-adapter` tests. Lives under `tests/bin/` so it ships as a
//! crate-internal binary that integration tests can locate via the
//! Cargo `CARGO_BIN_EXE_<name>` env var.
//!
//! Behaviour is keyed on `MOCK_CLI_FLAVOR` (default `claude` for
//! backwards compatibility with the WEK-13 integration tests).
//!
//! Supported invocations (only those the adapter tests need today):
//!
//! ```text
//! # Claude flavor (default)
//! mock-cli --version                # "2.1.158 (Claude Code)\n", exit 0
//! mock-cli --version --mode unauth  # stderr unauth message,        exit 1
//! mock-cli --version --mode garbage # unrelated text,                exit 0
//! mock-cli --print <prompt>         # echoes the prompt,             exit 0
//!
//! # Codex flavor (MOCK_CLI_FLAVOR=codex)
//! mock-cli --version                # "codex-cli 0.135.0\n",         exit 0
//! mock-cli --version --mode unauth  # stderr unauth message,         exit 1
//! mock-cli --version --mode garbage # unrelated text,                exit 0
//! mock-cli login status             # honours mode (exit 1 if unauth)
//! mock-cli exec --json <prompt>     # JSONL events,                  exit 0
//!
//! # Opencode flavor (MOCK_CLI_FLAVOR=opencode)
//! mock-cli --version                # "1.2.15\n",                    exit 0
//! mock-cli --version --mode unauth  # stderr unauth message,         exit 1
//! mock-cli --version --mode garbage # unrelated text,                exit 0
//! mock-cli auth list                # honours mode (prints "0 credentials" for unauth)
//! mock-cli run --format json <prompt> # JSONL events,                exit 0
//! ```
//!
//! Mode can also be supplied via env var `MOCK_CLI_MODE=ok|unauth|garbage`.

use std::env;
use std::io::Write;
use std::process::ExitCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
    Claude,
    Codex,
    Opencode,
}

fn flavor() -> Flavor {
    match env::var("MOCK_CLI_FLAVOR").as_deref() {
        Ok("codex") => Flavor::Codex,
        Ok("opencode") => Flavor::Opencode,
        _ => Flavor::Claude,
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let mode = pick_mode(&args)
        .unwrap_or_else(|| env::var("MOCK_CLI_MODE").unwrap_or_else(|_| "ok".into()));

    let subcmd = args.iter().find(|a| !a.starts_with("--mode")).cloned();
    let flavor = flavor();

    match (flavor, subcmd.as_deref()) {
        (_, Some("--version")) => version(flavor, &mode),
        (Flavor::Claude, Some("--print")) => print_prompt(&args),
        (Flavor::Codex, Some("login")) => codex_login(&args, &mode),
        (Flavor::Codex, Some("exec")) => codex_exec(&args),
        (Flavor::Opencode, Some("auth")) => opencode_auth(&args, &mode),
        (Flavor::Opencode, Some("run")) => opencode_run(&args),
        _ => {
            let _ = writeln!(
                std::io::stderr(),
                "mock-cli: unknown invocation: {:?}",
                args
            );
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

fn version(flavor: Flavor, mode: &str) -> ExitCode {
    match (flavor, mode) {
        (Flavor::Claude, "ok") => {
            println!("2.1.158 (Claude Code)");
            ExitCode::SUCCESS
        }
        (Flavor::Claude, "unauth") => {
            let _ = writeln!(
                std::io::stderr(),
                "Error: not logged in. Run `claude login` to authenticate."
            );
            ExitCode::from(1)
        }
        (Flavor::Claude, "garbage") => {
            println!("welcome to nothing in particular");
            ExitCode::SUCCESS
        }
        (Flavor::Codex, "ok") | (Flavor::Codex, "login_unsupported") => {
            // `login_unsupported` is a `login status`-only switch; for
            // `--version` it behaves identically to `ok` so the adapter's
            // secondary auth probe is exercised against a valid version
            // line. See `codex_login` for the login-side behaviour.
            println!("codex-cli 0.135.0");
            ExitCode::SUCCESS
        }
        (Flavor::Codex, "unauth") => {
            let _ = writeln!(
                std::io::stderr(),
                "Error: not logged in. Please run codex login."
            );
            ExitCode::from(1)
        }
        (Flavor::Codex, "garbage") => {
            println!("welcome to nothing");
            ExitCode::SUCCESS
        }
        (Flavor::Opencode, "ok") | (Flavor::Opencode, "auth_unsupported") => {
            // `auth_unsupported` is an `auth list`-only switch; for
            // `--version` it behaves identically to `ok` so the
            // adapter's secondary auth probe is exercised against a
            // valid version line.
            println!("1.2.15");
            ExitCode::SUCCESS
        }
        (Flavor::Opencode, "unauth") => {
            let _ = writeln!(
                std::io::stderr(),
                "Error: please run `opencode auth login`."
            );
            ExitCode::from(1)
        }
        (Flavor::Opencode, "garbage") => {
            println!("welcome to nothing");
            ExitCode::SUCCESS
        }
        (_, other) => {
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

fn codex_login(args: &[String], mode: &str) -> ExitCode {
    // Only `login status` is exercised by the adapter's secondary
    // auth probe; reject anything else loudly so test gaps surface.
    let sub = args.iter().skip_while(|a| *a != "login").nth(1).cloned();
    if sub.as_deref() != Some("status") {
        let _ = writeln!(
            std::io::stderr(),
            "mock-cli: unsupported `login` subcommand: {sub:?}"
        );
        return ExitCode::from(2);
    }
    match mode {
        "ok" => {
            println!("Logged in as mock@example.com");
            ExitCode::SUCCESS
        }
        "unauth" => {
            println!("Not logged in");
            ExitCode::from(1)
        }
        // Simulates an older / newer codex that doesn't recognise
        // `login status`: non-zero exit with NO unauth keyword. The
        // adapter must NOT misclassify this as Unauthenticated.
        "login_unsupported" => {
            let _ = writeln!(std::io::stderr(), "error: unrecognized subcommand 'status'");
            ExitCode::from(2)
        }
        other => {
            let _ = writeln!(std::io::stderr(), "mock-cli: unknown mode: {other}");
            ExitCode::from(3)
        }
    }
}

fn codex_exec(args: &[String]) -> ExitCode {
    // The prompt is the last positional argument after the flag block.
    // We tolerate (and ignore) `--json` / `--cd <dir>` / `-o <file>`.
    let mut prompt: Option<String> = None;
    let mut iter = args.iter().skip_while(|a| *a != "exec").skip(1);
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--json" => {}
            "--cd" | "-o" => {
                let _ = iter.next();
            }
            other => {
                prompt = Some(other.to_string());
            }
        }
    }
    let prompt = prompt.unwrap_or_default();
    println!("{{\"type\":\"task_started\"}}");
    println!(
        "{{\"type\":\"task_completed\",\"reply\":\"MOCK_REPLY: {}\"}}",
        prompt.replace('\\', "\\\\").replace('"', "\\\"")
    );
    ExitCode::SUCCESS
}

fn opencode_auth(args: &[String], mode: &str) -> ExitCode {
    // Only `auth list` is exercised by the adapter's secondary auth
    // probe; reject anything else loudly so test gaps surface.
    let sub = args.iter().skip_while(|a| *a != "auth").nth(1).cloned();
    if sub.as_deref() != Some("list") {
        let _ = writeln!(
            std::io::stderr(),
            "mock-cli: unsupported `auth` subcommand: {sub:?}"
        );
        return ExitCode::from(2);
    }
    match mode {
        "ok" => {
            println!("Credentials");
            println!("GitHub Copilot  oauth");
            println!("1 credentials");
            ExitCode::SUCCESS
        }
        "unauth" => {
            // Empty list — opencode prints a "0 credentials" footer.
            println!("Credentials");
            println!("0 credentials");
            ExitCode::SUCCESS
        }
        // Simulates an older / newer opencode that doesn't recognise
        // `auth list`: non-zero exit with NO unauth keyword. The
        // adapter must NOT misclassify this as Unauthenticated.
        "auth_unsupported" => {
            let _ = writeln!(std::io::stderr(), "error: unrecognized subcommand 'list'");
            ExitCode::from(2)
        }
        other => {
            let _ = writeln!(std::io::stderr(), "mock-cli: unknown mode: {other}");
            ExitCode::from(3)
        }
    }
}

fn opencode_run(args: &[String]) -> ExitCode {
    // The prompt is the last positional argument after the flag block.
    // We tolerate (and ignore) `--format json` / `--dir <dir>` /
    // `--session <id>` etc.
    let mut prompt: Option<String> = None;
    let mut iter = args.iter().skip_while(|a| *a != "run").skip(1);
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--format" | "--dir" | "--session" | "-s" | "-m" | "--model" | "--title" => {
                let _ = iter.next();
            }
            "--continue" | "-c" | "--share" | "--fork" | "--thinking" => {}
            other => {
                prompt = Some(other.to_string());
            }
        }
    }
    let prompt = prompt.unwrap_or_default();
    println!("{{\"type\":\"start\"}}");
    println!(
        "{{\"type\":\"done\",\"reply\":\"MOCK_REPLY: {}\"}}",
        prompt.replace('\\', "\\\\").replace('"', "\\\"")
    );
    ExitCode::SUCCESS
}
