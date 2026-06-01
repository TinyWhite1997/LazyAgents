//! `la-tui` — ratatui + crossterm widgets for the LazyAgents client `la`.
//!
//! This crate owns the conversation main area only (M1.6): the streaming PTY
//! transcript, scroll state, the composer, and the detach toast. The Sessions
//! sidebar (M1.5) lands in a sibling module and consumes the same widgets.
//!
//! ## Layering
//!
//! Per architecture §2.1, `la-tui` MUST stay a pure presentation crate:
//! - Allowed deps: `la-proto`, `la-ipc`, ratatui/crossterm/vte, plus the
//!   pure `unicode-width` helper.
//! - Forbidden deps: `la-core`, `la-storage`, `la-adapter`, `la-pty`. The
//!   server-side state and the PTY itself are reached only over RPC.
//!
//! ## Module map
//!
//! - [`vte_term`] — a [`vte::Perform`] implementation that folds PTY bytes
//!   into a line-oriented buffer, silently absorbing the cursor-query and
//!   OSC sequences ConPTY injects (architecture §6.5).
//! - [`transcript`] — append-only ring of rendered lines + scroll state with
//!   auto-follow (`Ctrl+u` / `Ctrl+d`, PgUp/PgDn, Home/End, G).
//! - [`composer`] — multi-line prompt editor with `Ctrl+Enter` send and
//!   `Up`/`Down` history recall.
//! - [`detach_notice`] — transient "会话仍在后台运行" toast surfaced when the
//!   user detaches from a live session.

pub mod composer;
pub mod detach_notice;
pub mod transcript;
pub mod vte_term;

pub use composer::{Composer, ComposerAction, ComposerView};
pub use detach_notice::{DetachNotice, DetachNoticeView};
pub use transcript::{ScrollAction, Transcript, TranscriptView};
pub use vte_term::{StyledCell, TerminalLine, TerminalScreen};
