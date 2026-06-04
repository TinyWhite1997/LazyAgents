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
//! Sessions sidebar (M1.5):
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
//! Conversation main area (M1.6):
//! - [`vte_term`] — a [`vte::Perform`] implementation that folds PTY bytes
//!   into a line-oriented buffer, silently absorbing the cursor-query and
//!   OSC sequences ConPTY injects (architecture §6.5).
//! - [`transcript`] — append-only ring of rendered lines + scroll state with
//!   auto-follow (`Ctrl+u` / `Ctrl+d`, PgUp/PgDn, Home/End, G).
//! - [`composer`] — multi-line prompt editor with `Ctrl+Enter` send and
//!   `Up`/`Down` history recall.
//! - [`detach_notice`] — transient "会话仍在后台运行" toast surfaced when the
//!   user detaches from a live session.
//!
//! Crons tab (M3.4):
//! - [`crons`] — list state + editor form + cron-expression preview
//!   (`下次：明日 02:00`) + dry-run modal that lists the next 5 fire times.
//!   The preview shares the workspace-pinned `cron` + `chrono-tz` versions
//!   with [`la-scheduler::cron_spec`], so the editor's preview is
//!   byte-identical to what the daemon will eventually schedule.
//!
//! The binary entry point is in `src/bin/la.rs`.

pub mod app;
pub mod bootstrap;
pub mod composer;
pub mod crons;
pub mod detach_notice;
pub mod diff_view;
pub mod health_sub;
pub mod input;
pub mod key_hints;
pub mod model;
pub mod notif_sub;
pub mod runner;
pub mod sidebar;
pub mod source;
pub mod status;
pub mod tabs;
pub mod theme;
pub mod transcript;
pub mod ui_prefs;
pub mod update_check;
pub mod vte_term;

pub use app::{App, AppMsg, Focus, Tab};
pub use composer::{Composer, ComposerAction, ComposerView};
pub use crons::{Cron, CronPreview, CronSource, CronsState, EditField, FieldEdit, MockCronSource};
pub use detach_notice::{DetachNotice, DetachNoticeView};
pub use diff_view::{
    DiffAction, DiffEvent, DiffFileState, DiffFocus, DiffKey, DiffModal, DiffPayload, DiffSource,
    DiffView, DiffViewWidget, DiscardScope, MockDiffSource,
};
pub use model::{Backend, BackendBadge, ProjectGroup, RunState, SessionRow};
pub use notif_sub::{HealthEvent, HealthSnapshot, NotifEvent};
pub use source::{MockSessionSource, SessionSource};
pub use status::{CronPulse, Status};
pub use theme::{Accent, KeyHintsMode, Palette, Theme};
pub use transcript::{ScrollAction, Transcript, TranscriptView};
pub use ui_prefs::UiPrefs;
pub use vte_term::{StyledCell, TerminalLine, TerminalScreen};
