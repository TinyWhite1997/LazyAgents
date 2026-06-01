//! TUI client (`la`) for LazyAgents.
//!
//! This crate is the keyboard-driven terminal client; per architecture §2.1
//! it depends **only** on `la-proto` (wire types) and `la-ipc` (transport).
//! It never reaches into `la-core` or `la-storage` directly — the daemon is
//! the single owner of session state, and the TUI is a thin renderer plus
//! input router on top of it.
//!
//! ## Layout
//!
//! - [`model`] — wire-decoupled data types used by the sidebar (project
//!   group, session row, backend badge, run-state glyph). They mirror the
//!   subset of [`la_proto::methods::SessionSummary`] the sidebar needs and
//!   add UI-only fields (the run-state icon, the archive bucket).
//! - [`source`] — [`source::SessionSource`] trait abstracting "where do
//!   sessions come from": tests use [`source::MockSessionSource`]; the `la`
//!   binary will swap in an IPC-backed implementation once the daemon
//!   (M1.7) exists. Until then the binary uses the mock too.
//! - [`sidebar`] — the navigation state machine ([`sidebar::SidebarState`]:
//!   selection, fold/unfold, archive bucket, j/k/g/G/h/l semantics) and the
//!   ratatui widget that renders it.
//! - [`key_hints`] — context-driven [`key_hints::HintRegistry`] (PRD §5.6
//!   渐进披露 / 重要性排序) and the `?` which-key overlay.
//! - [`tabs`] — top tab bar (Sessions / Crons; `Tab` / `Shift+Tab` / digit
//!   shortcuts; mouse click).
//! - [`status`] — bottom status line (daemon health badge, running count,
//!   next cron preview placeholder).
//! - [`app`] — the [`app::App`] that owns sidebar + tab + modal-confirm
//!   state and translates input into state changes.
//! - [`input`] — crossterm `Event` → [`app::AppMsg`] translator.
//! - [`runner`] — minimal event loop: render → wait for event → dispatch.
//!
//! The binary entry point is in `src/bin/la.rs`.

pub mod app;
pub mod input;
pub mod key_hints;
pub mod model;
pub mod runner;
pub mod sidebar;
pub mod source;
pub mod status;
pub mod tabs;

pub use app::{App, AppMsg, Focus, Tab};
pub use model::{Backend, ProjectGroup, RunState, SessionRow};
pub use source::{MockSessionSource, SessionSource};
pub use status::Status;
