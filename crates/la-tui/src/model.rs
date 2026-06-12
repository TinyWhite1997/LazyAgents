//! UI-facing data types for the Sessions sidebar.
//!
//! Kept independent from `la_proto` wire shapes so the sidebar can be
//! unit-tested without dragging in the JSON-RPC machinery, and so the
//! UI-only fields (run-state glyph color, archive bucket) don't leak into
//! the protocol crate.
//!
//! [`SessionRow::from_summary`] is the only bridge in this direction:
//! everything else in this crate consumes [`SessionRow`] / [`ProjectGroup`]
//! and is wire-agnostic.

use la_proto::methods::{SessionState, SessionSummary};
use la_proto::notifications::{BackendHealth as WireBackendHealth, BackendHealthStatus};

/// Backend identifier as it should appear in the sidebar badge.
///
/// We keep the raw `String` (`backend.id()`) instead of an enum because
/// adapters are dynamically registered (architecture §4.4 加新 adapter 的
/// 成本) — hard-coding a closed variant set here would force a re-deploy of
/// the TUI to display a new backend's badge.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Backend {
    id: String,
}

impl Backend {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Short human label shown in the sidebar row.
    ///
    /// Capped at 8 chars to keep alignment with the run-state glyph; longer
    /// adapter ids fall back to a 7-char prefix + `…`.
    pub fn label(&self) -> String {
        const MAX: usize = 8;
        if self.id.chars().count() <= MAX {
            self.id.clone()
        } else {
            let mut s: String = self.id.chars().take(MAX - 1).collect();
            s.push('…');
            s
        }
    }
}

/// Run-state glyph + colour bucket for the sidebar.
///
/// Glyphs: `●` running / `○` idle / `⏸` waiting input / `✗` errored /
/// `·` exited. Colours follow the PRD §5.3 status palette: blue=running,
/// green=complete (exited & waiting), grey=idle, red=errored.
///
/// We compress the protocol's six states ([`SessionState`]) down to five
/// presentation buckets (`Archived` is folded into the dedicated Archived
/// group instead of a glyph).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Running,
    Idle,
    Waiting,
    Errored,
    Exited,
}

impl RunState {
    pub fn glyph(self) -> &'static str {
        match self {
            RunState::Running => "●",
            RunState::Idle => "○",
            RunState::Waiting => "⏸",
            RunState::Errored => "✗",
            RunState::Exited => "·",
        }
    }

    /// Map a wire [`SessionState`] to the sidebar glyph bucket.
    ///
    /// `Archived` is intentionally NOT representable here — callers route
    /// archived rows to the Archived group ([`SessionRow::archived`]) and
    /// keep this enum for the visible glyph.
    pub fn from_state(state: SessionState) -> Self {
        match state {
            SessionState::Starting | SessionState::Running => RunState::Running,
            SessionState::Waiting => RunState::Waiting,
            SessionState::Errored => RunState::Errored,
            SessionState::Exited => RunState::Exited,
            // Archived rows never reach the glyph path — fall back to a
            // neutral marker if a caller mis-routes one.
            SessionState::Archived => RunState::Exited,
        }
    }
}

/// One row in the sidebar: a session under a project group.
///
/// Holds only the fields needed to draw the row + drive `enter / d / a` —
/// detail fetches happen against the daemon on demand, not via this struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub session_id: String,
    pub project_id: String,
    pub backend: Backend,
    pub title: Option<String>,
    pub run_state: RunState,
    /// Soft-deleted by the user — placed under the Archived bucket and
    /// hidden from per-project glyph computation.
    pub archived: bool,
    /// Surfaced by `adapters.discover` but not yet imported. The row
    /// lives under the synthetic Discovered bucket and is read-only —
    /// only the `i` (import) action applies. `session_id` here is the
    /// backend's own `external_id`; the daemon assigns a fresh row on
    /// import.
    pub discovered: bool,
}

impl SessionRow {
    /// Short display title: user-set title, otherwise the backend label + the
    /// session id's last 6 chars (UUID-v7 tail keeps creation order obvious).
    pub fn display_title(&self) -> String {
        if let Some(t) = &self.title {
            return t.clone();
        }
        let tail: String = self
            .session_id
            .chars()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        format!("{} · {}", self.backend.label(), tail)
    }

    /// Bridge from the wire summary returned by `sessions.list`.
    pub fn from_summary(s: &SessionSummary) -> Self {
        Self {
            session_id: s.session_id.clone(),
            project_id: s.project_id.clone(),
            backend: Backend::new(&s.backend),
            title: s.title.clone(),
            run_state: RunState::from_state(s.state),
            archived: matches!(s.state, SessionState::Archived),
            discovered: false,
        }
    }
}

/// One sidebar group: a project directory with its sessions, plus a fold
/// flag and a display root path.
///
/// The Archived bucket is also a [`ProjectGroup`] with [`is_archived`] set;
/// keeping the same type means the navigation code does not need to special-
/// case it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectGroup {
    pub project_id: String,
    /// Short label shown in the header (usually the directory basename).
    pub display_name: String,
    /// Full path shown on hover / in the status bar.
    pub root_path: String,
    /// `false` ⇒ children hidden in the navigation flow.
    pub expanded: bool,
    pub sessions: Vec<SessionRow>,
    /// Marks the synthetic "Archived" bucket so renderers can pin it at the
    /// bottom (PRD §5.3: "末尾固定 Archived 分组").
    pub is_archived: bool,
}

impl ProjectGroup {
    pub fn new(project_id: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            project_id: project_id.into(),
            display_name: display_name.into(),
            root_path: String::new(),
            expanded: true,
            sessions: Vec::new(),
            is_archived: false,
        }
    }

    /// The synthetic Archived bucket.
    ///
    /// `project_id` is a fixed sentinel ([`Self::ARCHIVED_ID`]) so the
    /// navigation never confuses it with a real project; PRD says the bucket
    /// is collapsed by default ("可展开恢复" implies starts folded).
    pub fn archived() -> Self {
        Self {
            project_id: Self::ARCHIVED_ID.to_string(),
            display_name: "Archived".to_string(),
            root_path: String::new(),
            expanded: false,
            sessions: Vec::new(),
            is_archived: true,
        }
    }

    /// Sentinel project id for the Archived bucket. Reserved — the daemon
    /// must never assign this UUID-shaped string to a real project.
    pub const ARCHIVED_ID: &'static str = "__archived__";

    /// The synthetic Discovered bucket (WEK-26 / M2.3).
    ///
    /// Lists sessions surfaced by `adapters.discover` that the user has
    /// not yet promoted via `sessions.import`. Rows here are read-only
    /// — the only action is `i` (import). The bucket starts expanded so
    /// fresh discoveries are visible without a key press; a renderer
    /// pins it just above the Archived bucket so the project-list
    /// order isn't disturbed.
    pub fn discovered() -> Self {
        Self {
            project_id: Self::DISCOVERED_ID.to_string(),
            display_name: "Discovered".to_string(),
            root_path: String::new(),
            expanded: true,
            sessions: Vec::new(),
            is_archived: false,
        }
    }

    /// Sentinel project id for the Discovered bucket. Reserved.
    pub const DISCOVERED_ID: &'static str = "__discovered__";

    /// `true` when this group is the synthetic Discovered bucket.
    pub fn is_discovered(&self) -> bool {
        self.project_id == Self::DISCOVERED_ID
    }
}

/// UI-side projection of a [`la_proto::notifications::BackendHealth`].
///
/// The wire type carries everything the TUI needs, but it speaks JSON-RPC
/// snake_case and has fields the sidebar doesn't render (`last_probed_at`
/// for now). Keeping a slim UI-side shape lets the sidebar stay agnostic
/// to the protocol version and lets tests build fixtures without quoting
/// strings into a JSON envelope first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendBadge {
    /// Stable adapter id (`"claude"` / `"codex"` / `"opencode"`).
    pub id: String,
    /// Human-readable label shown in the sidebar header.
    pub display_name: String,
    pub status: BackendHealthStatus,
    /// One-line reason; non-empty only for non-Available states.
    pub reason: Option<String>,
    /// Docs / install / login link for the user to follow when the
    /// backend is grey-stated. None for Available.
    pub docs_url: Option<String>,
    /// Version string for Available backends (e.g. `"2.1.158"`).
    pub version: Option<String>,
}

impl BackendBadge {
    /// Bridge from the wire payload pushed via `daemon.health`.
    pub fn from_wire(w: &WireBackendHealth) -> Self {
        Self {
            id: w.id.clone(),
            display_name: w.display_name.clone(),
            status: w.status,
            reason: w.reason.clone(),
            docs_url: w.docs_url.clone(),
            version: w.version.clone(),
        }
    }

    /// `true` when the backend cannot accept a fresh `sessions.create`.
    /// Only `NotInstalled` (and the genuinely broken `ProtocolDrift` /
    /// `Error` states) qualify. `Unauthenticated` is *available* — the
    /// CLI can run against an API key, so it stays offered in the
    /// new-session picker and is not flagged as an error.
    pub fn is_unavailable(&self) -> bool {
        matches!(
            self.status,
            BackendHealthStatus::NotInstalled
                | BackendHealthStatus::ProtocolDrift
                | BackendHealthStatus::Error
        )
    }

    /// Glyph shown next to the backend name. Each variant has its own
    /// dedicated character so screenshots of the sidebar stay greppable.
    pub fn glyph(&self) -> &'static str {
        match self.status {
            BackendHealthStatus::Available => "●",
            BackendHealthStatus::NotInstalled => "○",
            BackendHealthStatus::Unauthenticated => "◐",
            BackendHealthStatus::ProtocolDrift => "▲",
            BackendHealthStatus::Error => "✗",
        }
    }

    /// Short label suitable for inline display (`"available"` /
    /// `"not installed"` / etc.).
    pub fn status_label(&self) -> &'static str {
        match self.status {
            BackendHealthStatus::Available => "available",
            BackendHealthStatus::NotInstalled => "not installed",
            BackendHealthStatus::Unauthenticated => "not logged in",
            BackendHealthStatus::ProtocolDrift => "protocol drift",
            BackendHealthStatus::Error => "error",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_label_truncates_long_ids() {
        assert_eq!(Backend::new("claude").label(), "claude");
        assert_eq!(Backend::new("opencode").label(), "opencode"); // 8 == cap
        assert_eq!(Backend::new("verylongadapter").label(), "verylon…");
    }

    #[test]
    fn run_state_maps_wire_states() {
        assert_eq!(
            RunState::from_state(SessionState::Running),
            RunState::Running
        );
        assert_eq!(
            RunState::from_state(SessionState::Waiting),
            RunState::Waiting
        );
        assert_eq!(
            RunState::from_state(SessionState::Errored),
            RunState::Errored
        );
        // Archived rows go to the bucket; the glyph is a fallback.
        assert_eq!(
            RunState::from_state(SessionState::Archived),
            RunState::Exited
        );
    }

    #[test]
    fn row_display_title_falls_back_to_backend_and_tail() {
        let s = SessionRow {
            session_id: "01934fff-feed-7000-a000-abcdefabcdef".to_string(),
            project_id: "p1".to_string(),
            backend: Backend::new("claude"),
            title: None,
            run_state: RunState::Idle,
            archived: false,
            discovered: false,
        };
        assert_eq!(s.display_title(), "claude · abcdef");
    }

    #[test]
    fn row_display_title_prefers_user_title() {
        let s = SessionRow {
            session_id: "x".to_string(),
            project_id: "p1".to_string(),
            backend: Backend::new("claude"),
            title: Some("Fix login bug".to_string()),
            run_state: RunState::Idle,
            archived: false,
            discovered: false,
        };
        assert_eq!(s.display_title(), "Fix login bug");
    }

    #[test]
    fn backend_badge_from_wire_carries_status_and_links() {
        let wire = WireBackendHealth {
            id: "codex".into(),
            display_name: "Codex CLI".into(),
            status: BackendHealthStatus::NotInstalled,
            version: None,
            reason: Some("`codex` not on $PATH".into()),
            docs_url: Some("https://example.com/install/codex".into()),
            last_probed_at: "2026-06-02T00:00:00Z".into(),
        };
        let badge = BackendBadge::from_wire(&wire);
        assert!(badge.is_unavailable());
        assert_eq!(badge.glyph(), "○");
        assert_eq!(badge.status_label(), "not installed");
        assert_eq!(badge.reason.as_deref(), Some("`codex` not on $PATH"));
        assert_eq!(
            badge.docs_url.as_deref(),
            Some("https://example.com/install/codex")
        );
    }

    #[test]
    fn backend_badge_available_is_not_grey_state() {
        let badge = BackendBadge::from_wire(&WireBackendHealth {
            id: "claude".into(),
            display_name: "Claude Code".into(),
            status: BackendHealthStatus::Available,
            version: Some("2.1.158".into()),
            reason: None,
            docs_url: None,
            last_probed_at: "2026-06-02T00:00:00Z".into(),
        });
        assert!(!badge.is_unavailable());
        assert_eq!(badge.glyph(), "●");
        assert_eq!(badge.version.as_deref(), Some("2.1.158"));
    }

    #[test]
    fn backend_badge_unauthenticated_is_available() {
        // API-key mode means an unauthenticated CLI is still usable, so it
        // must not be grey-stated / dropped from the new-session picker.
        let badge = BackendBadge::from_wire(&WireBackendHealth {
            id: "codex".into(),
            display_name: "Codex CLI".into(),
            status: BackendHealthStatus::Unauthenticated,
            version: None,
            reason: Some("not logged in".into()),
            docs_url: Some("https://example.com/login".into()),
            last_probed_at: "2026-06-02T00:00:00Z".into(),
        });
        assert!(!badge.is_unavailable());
        assert_eq!(badge.glyph(), "◐");
        assert_eq!(badge.status_label(), "not logged in");
    }

    #[test]
    fn backend_badge_protocol_drift_surfaces_upgrade_hint() {
        let badge = BackendBadge::from_wire(&WireBackendHealth {
            id: "opencode".into(),
            display_name: "OpenCode".into(),
            status: BackendHealthStatus::ProtocolDrift,
            version: Some("9.9.9-future".into()),
            reason: Some("could not parse version line".into()),
            docs_url: Some("https://opencode.ai/docs/".into()),
            last_probed_at: "2026-06-02T00:00:00Z".into(),
        });
        assert!(badge.is_unavailable());
        // ProtocolDrift is distinct from NotInstalled / Unauthenticated
        // so screenshots stay greppable — pin the glyph here.
        assert_eq!(badge.glyph(), "▲");
        assert_eq!(badge.status_label(), "protocol drift");
    }
}
