//! `la` — LazyAgents TUI client.
//!
//! For M1.5 we ship the binary against an in-memory [`MockSessionSource`]:
//! the daemon (M1.7) is not yet on `main`, and the dependency rule (la-tui
//! depends only on la-proto + la-ipc, architecture §2.1) means we can't
//! reach across to la-core. Once the daemon lands, swapping the source for
//! an IPC-backed implementation is the only change needed here.

use std::io;
use std::process::ExitCode;

use la_tui::status::Status;
use la_tui::{App, AppMsg, MockSessionSource};

fn main() -> ExitCode {
    if let Err(e) = real_main() {
        eprintln!("la: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

fn real_main() -> io::Result<()> {
    // For M1.5 we always use the mock fixture. A future flag (--demo)
    // will toggle it once a real source exists; the binary is intentionally
    // tiny so it doesn't accumulate scaffolding the daemon swap will
    // throw away.
    let source = MockSessionSource::fixture();
    let mut app = App::new(source);
    // Seed the status bar with a plausible snapshot so the demo binary
    // shows the layout reviewers want to evaluate.
    app.handle(AppMsg::StatusUpdate(Status {
        daemon_online: false,
        running: 2,
        next_cron_label: Some("cron pane in M3".to_string()),
        right_context: "demo mode (no daemon)".to_string(),
    }));
    la_tui::runner::run(app)
}
