//! Diff review view for the la-tui main area (M2.5 / WEK-28).
//!
//! Wired around the `worktree.*` RPC surface defined in la-proto:
//!
//! - `worktree.status` populates the file list on first entry and
//!   after every `worktree.changed` notification.
//! - `worktree.diff` is fired lazily when the user expands a file
//!   fold; hunks are cached on the [`DiffFileState`] so re-expanding
//!   doesn't re-pay the round trip.
//! - `worktree.stage` / `worktree.unstage` / `worktree.commit` /
//!   `worktree.discard` are fired in response to `s`, `x`, `c`, and
//!   `R Y` respectively. Discard always goes through the
//!   [`ConfirmDiscard`] modal (PRD §5.3 二次确认 acceptance).
//! - `worktree.open_in_editor` fires on `o`; the daemon spawns the
//!   editor without taking the alt screen so the TUI does not have to
//!   suspend.
//!
//! This module is deliberately self-contained — the source trait
//! [`DiffSource`] is implemented in the daemon-backed binary; tests use
//! the in-memory [`MockDiffSource`].

use la_proto::methods::{DiffOrigin, FileEntry, Hunk, TruncationMarker};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Widget, Wrap};

/// Pluggable data source for the diff view. The production impl in
/// `crates/la-tui/src/bin/la.rs` (or wherever the daemon-backed source
/// lives) calls the matching RPCs over IPC; tests use
/// [`MockDiffSource`].
///
/// The trait is intentionally sync because the view's keystroke handler
/// is sync. Daemon-backed implementations can either block on a current
/// runtime or, more realistically, push the call to a background task
/// and refresh the view via `apply_status` once the response lands —
/// this trait is the contract for "what the view asks for", not "how it
/// gets fulfilled".
pub trait DiffSource {
    /// Refresh the per-file snapshot. Called on first entry to the
    /// diff view and whenever a `worktree.changed` notification lands.
    /// Synchronous interface returns `Vec<FileEntry>` — the daemon
    /// adapter may return a stale snapshot when its request is still
    /// in flight.
    fn snapshot(&self) -> Vec<FileEntry>;

    /// Request the diff for one file. Synchronous in the trait but the
    /// daemon implementation may return `None` on a cache miss and
    /// kick off an async fetch; the view will re-query on the next
    /// frame.
    fn diff_file(&self, path: &str, staged: bool) -> Option<DiffPayload>;

    fn stage(&self, hunk_ids: &[String]);
    fn unstage(&self, hunk_ids: &[String]);
    fn discard(&self, hunk_ids: &[String], confirmed: bool);
    fn commit(&self, message: &str);
    fn open_in_editor(&self, path: &str, line: Option<u32>);
}

#[derive(Debug, Clone)]
pub struct DiffPayload {
    pub file: FileEntry,
    pub hunks: Vec<Hunk>,
    pub truncated: Option<TruncationMarker>,
}

/// Focus inside the diff view: either the file list (default) or the
/// hunk list on the right pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffFocus {
    Files,
    Hunks,
}

/// What's open over the diff view: nothing, the commit message editor,
/// or the discard confirmation dialog (PRD §5.3 二次确认 acceptance).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffModal {
    None,
    Commit { draft: String },
    ConfirmDiscard { scope: DiscardScope },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscardScope {
    Hunk { path: String, hunk_id: String },
    File { path: String },
}

/// Per-file local state.
#[derive(Debug, Clone)]
pub struct DiffFileState {
    pub entry: FileEntry,
    pub expanded: bool,
    pub hunks: Vec<Hunk>,
    pub truncated: Option<TruncationMarker>,
    /// `true` when we've received a payload (even if `hunks` is empty
    /// because the file is binary / truncated). Distinguishes "not
    /// yet loaded" from "loaded; nothing to show".
    pub loaded: bool,
    /// Cursor inside `hunks`; only meaningful when `focus == Hunks`
    /// and `expanded`.
    pub hunk_cursor: usize,
}

impl DiffFileState {
    fn new(entry: FileEntry) -> Self {
        Self {
            entry,
            expanded: false,
            hunks: Vec::new(),
            truncated: None,
            loaded: false,
            hunk_cursor: 0,
        }
    }
}

/// Top-level state of the diff view.
#[derive(Debug, Clone)]
pub struct DiffView {
    pub files: Vec<DiffFileState>,
    pub file_cursor: usize,
    pub focus: DiffFocus,
    pub modal: DiffModal,
    /// Banner shown in the status bar after the last RPC outcome — set
    /// by [`Self::push_toast`], cleared by [`Self::clear_toast`].
    pub toast: Option<String>,
}

pub struct DiffViewWidget<'a> {
    view: &'a DiffView,
}

impl<'a> DiffViewWidget<'a> {
    pub fn new(view: &'a DiffView) -> Self {
        Self { view }
    }
}

impl Widget for DiffViewWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::default().borders(Borders::ALL).title("Diff");
        let inner = block.inner(area);
        block.render(area, buf);

        if inner.width < 8 || inner.height < 3 {
            return;
        }

        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(34), Constraint::Percentage(66)])
            .split(inner);

        let file_items: Vec<ListItem<'_>> = self
            .view
            .files
            .iter()
            .enumerate()
            .map(|(idx, file)| {
                let marker = if idx == self.view.file_cursor {
                    ">"
                } else {
                    " "
                };
                let fold = if file.expanded { "v" } else { ">" };
                let line = Line::from(vec![
                    Span::raw(format!("{marker} {fold} ")),
                    Span::styled(
                        file.entry.path.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!(
                        " +{} ~{} {:?}",
                        file.entry.staged_hunks, file.entry.unstaged_hunks, file.entry.status
                    )),
                ]);
                ListItem::new(line)
            })
            .collect();
        List::new(file_items)
            .block(Block::default().borders(Borders::RIGHT).title("Files"))
            .render(chunks[0], buf);

        let detail = self
            .view
            .files
            .get(self.view.file_cursor)
            .map(render_file_detail)
            .unwrap_or_else(|| vec![Line::from("No changes")]);
        Paragraph::new(detail)
            .block(Block::default().title("Hunks"))
            .wrap(Wrap { trim: false })
            .render(chunks[1], buf);
    }
}

fn render_file_detail(file: &DiffFileState) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(vec![
        Span::styled(
            file.entry.path.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " staged={} unstaged={}",
            file.entry.staged_hunks, file.entry.unstaged_hunks
        )),
    ])];

    if let Some(truncated) = &file.truncated {
        lines.push(Line::from(format!(
            "truncated: {} {} bytes ({})",
            truncated.reason, truncated.size_bytes, truncated.hint
        )));
        return lines;
    }

    if file.hunks.is_empty() {
        lines.push(Line::from(if file.loaded {
            "No hunks"
        } else {
            "Expand a file to load hunks"
        }));
        return lines;
    }

    for hunk in &file.hunks {
        lines.push(Line::from(Span::styled(
            hunk.header.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for line in hunk.lines.iter().take(8) {
            lines.push(Line::from(format!(
                "{}{}",
                diff_origin_prefix(line.origin),
                line.content
            )));
        }
    }
    lines
}

fn diff_origin_prefix(origin: DiffOrigin) -> &'static str {
    match origin {
        DiffOrigin::Context => " ",
        DiffOrigin::Add => "+",
        DiffOrigin::Delete => "-",
    }
}

impl Default for DiffView {
    fn default() -> Self {
        Self::new()
    }
}

impl DiffView {
    pub fn new() -> Self {
        Self {
            files: Vec::new(),
            file_cursor: 0,
            focus: DiffFocus::Files,
            modal: DiffModal::None,
            toast: None,
        }
    }

    /// Replace the per-file snapshot. Preserves the `expanded` /
    /// `hunks` / `hunk_cursor` of any file that survives the refresh
    /// (matched by path) so a `worktree.changed` event doesn't collapse
    /// folds the user opened.
    pub fn apply_status(&mut self, entries: Vec<FileEntry>) {
        let mut keep: std::collections::HashMap<String, DiffFileState> = self
            .files
            .drain(..)
            .map(|f| (f.entry.path.clone(), f))
            .collect();
        let mut next = Vec::with_capacity(entries.len());
        for e in entries {
            let path = e.path.clone();
            if let Some(mut prev) = keep.remove(&path) {
                prev.entry = e;
                // If the underlying file changed, the cached hunks may
                // be stale; mark them as unloaded so the next expand
                // re-fetches.
                prev.loaded = false;
                next.push(prev);
            } else {
                next.push(DiffFileState::new(e));
            }
        }
        self.files = next;
        if self.file_cursor >= self.files.len() {
            self.file_cursor = self.files.len().saturating_sub(1);
        }
    }

    /// Land a `worktree.diff` payload on the matching file.
    pub fn apply_diff(&mut self, payload: DiffPayload) {
        if let Some(file) = self
            .files
            .iter_mut()
            .find(|f| f.entry.path == payload.file.path)
        {
            file.entry = payload.file;
            file.hunks = payload.hunks;
            file.truncated = payload.truncated;
            file.loaded = true;
            if file.hunk_cursor >= file.hunks.len() {
                file.hunk_cursor = 0;
            }
        }
    }

    pub fn move_down(&mut self) {
        match self.focus {
            DiffFocus::Files => {
                if self.file_cursor + 1 < self.files.len() {
                    self.file_cursor += 1;
                }
            }
            DiffFocus::Hunks => {
                if let Some(file) = self.files.get_mut(self.file_cursor) {
                    if file.hunk_cursor + 1 < file.hunks.len() {
                        file.hunk_cursor += 1;
                    }
                }
            }
        }
    }

    pub fn move_up(&mut self) {
        match self.focus {
            DiffFocus::Files => {
                self.file_cursor = self.file_cursor.saturating_sub(1);
            }
            DiffFocus::Hunks => {
                if let Some(file) = self.files.get_mut(self.file_cursor) {
                    file.hunk_cursor = file.hunk_cursor.saturating_sub(1);
                }
            }
        }
    }

    /// Toggle the current file's fold; if collapsing, no fetch is
    /// needed. If expanding and the file's `hunks` haven't been loaded
    /// yet, the caller should issue a `worktree.diff` request.
    /// Returns `Some(path)` when a fetch is required, `None`
    /// otherwise.
    pub fn toggle_expand(&mut self) -> Option<String> {
        let file = self.files.get_mut(self.file_cursor)?;
        file.expanded = !file.expanded;
        if file.expanded && !file.loaded {
            return Some(file.entry.path.clone());
        }
        None
    }

    pub fn cycle_focus(&mut self) {
        self.focus = match self.focus {
            DiffFocus::Files => DiffFocus::Hunks,
            DiffFocus::Hunks => DiffFocus::Files,
        };
    }

    /// Return the currently-focused hunk id, if any. Used by `s` / `x`
    /// / `R` to know which hunk to operate on.
    pub fn current_hunk_id(&self) -> Option<(String, String)> {
        let file = self.files.get(self.file_cursor)?;
        let hunk = file.hunks.get(file.hunk_cursor)?;
        Some((file.entry.path.clone(), hunk.hunk_id.clone()))
    }

    /// Return the currently-focused file path, if any.
    pub fn current_file_path(&self) -> Option<String> {
        self.files
            .get(self.file_cursor)
            .map(|f| f.entry.path.clone())
    }

    pub fn open_commit_modal(&mut self) {
        if matches!(self.modal, DiffModal::None) {
            self.modal = DiffModal::Commit {
                draft: String::new(),
            };
        }
    }

    /// Request a discard on the currently-focused hunk (or whole file
    /// when no hunks are loaded). Opens the confirmation modal — the
    /// actual `worktree.discard` RPC fires only after `confirm_discard`.
    pub fn request_discard_hunk(&mut self) {
        if let Some((path, hunk_id)) = self.current_hunk_id() {
            self.modal = DiffModal::ConfirmDiscard {
                scope: DiscardScope::Hunk { path, hunk_id },
            };
        } else if let Some(path) = self.current_file_path() {
            self.modal = DiffModal::ConfirmDiscard {
                scope: DiscardScope::File { path },
            };
        }
    }

    /// Resolve the discard modal: returns the hunk ids to send to the
    /// daemon if the user pressed `Y`. The TUI then calls
    /// `source.discard(ids, confirmed = true)`. Pressing anything else
    /// → `None` (and the modal is also cleared by `cancel_modal`).
    pub fn confirm_discard(&mut self) -> Option<(Vec<String>, String)> {
        let DiffModal::ConfirmDiscard { scope } =
            std::mem::replace(&mut self.modal, DiffModal::None)
        else {
            return None;
        };
        match scope {
            DiscardScope::Hunk { path, hunk_id } => Some((vec![hunk_id], path)),
            DiscardScope::File { path } => {
                // Collect every hunk id for the file. If hunks aren't
                // loaded the caller will get an empty list — daemon
                // treats that as a no-op which is the safest fallback.
                let ids = self
                    .files
                    .iter()
                    .find(|f| f.entry.path == path)
                    .map(|f| f.hunks.iter().map(|h| h.hunk_id.clone()).collect())
                    .unwrap_or_default();
                Some((ids, path))
            }
        }
    }

    pub fn cancel_modal(&mut self) {
        self.modal = DiffModal::None;
    }

    pub fn commit_draft_push(&mut self, c: char) {
        if let DiffModal::Commit { draft } = &mut self.modal {
            draft.push(c);
        }
    }

    pub fn commit_draft_backspace(&mut self) {
        if let DiffModal::Commit { draft } = &mut self.modal {
            draft.pop();
        }
    }

    /// Yield the committed message and clear the modal. Returns `None`
    /// when the modal isn't `Commit` or the draft is empty (the daemon
    /// would reject an empty message and the TUI should keep the modal
    /// open instead).
    pub fn take_commit_message(&mut self) -> Option<String> {
        let DiffModal::Commit { draft } = &mut self.modal else {
            return None;
        };
        let msg = std::mem::take(draft);
        if msg.trim().is_empty() {
            self.modal = DiffModal::Commit { draft: msg };
            return None;
        }
        self.modal = DiffModal::None;
        Some(msg)
    }

    pub fn push_toast(&mut self, s: impl Into<String>) {
        self.toast = Some(s.into());
    }

    pub fn clear_toast(&mut self) {
        self.toast = None;
    }
}

/// Reasons the host App may pass to the diff view to indicate what
/// just happened. The view uses these to drive its own state machine
/// (e.g. mark a fetch as in flight, update the toast).
#[derive(Debug, Clone)]
pub enum DiffEvent {
    /// User pressed a key that maps to a diff action.
    Key(DiffKey),
    /// `worktree.status` came back.
    Status(Vec<FileEntry>),
    /// `worktree.diff` came back.
    Diff(DiffPayload),
    /// `worktree.commit` succeeded.
    CommitOk { commit_sha: String, summary: String },
    /// `worktree.changed` notification arrived from the daemon.
    Changed,
}

/// Keys the diff view consumes. The translator in `crate::input` maps
/// raw crossterm events to these before the dispatch reaches
/// [`DiffView::handle_key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKey {
    Up,
    Down,
    ToggleFold,
    CycleFocus,
    Stage,
    Unstage,
    Discard,
    Commit,
    OpenEditor,
    Exit,
}

/// Outcome the App returns to the runner after a key — mostly used to
/// thread "fire this RPC asynchronously" hints back to the daemon
/// adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffAction {
    /// Nothing for the runner to do.
    None,
    /// Fetch hunks for this path (`worktree.diff`).
    FetchDiff {
        path: String,
        staged: bool,
    },
    /// Stage / unstage the given hunk.
    Stage {
        hunk_id: String,
    },
    Unstage {
        hunk_id: String,
    },
    /// Open editor at this path.
    OpenEditor {
        path: String,
        line: Option<u32>,
    },
    /// Discard hunks (called only after the modal confirmation).
    Discard {
        hunk_ids: Vec<String>,
        path: String,
    },
    /// Commit the given message (called from the Commit modal after
    /// the user pressed Enter).
    Commit {
        message: String,
    },
    /// Pop back to the previous main view.
    Exit,
}

impl DiffView {
    /// Pure-function input handler. Returns the action the runner
    /// should fulfil (most actions translate to an RPC against the
    /// daemon source).
    ///
    /// Modal-aware: while a modal is open, keys are routed to the
    /// modal's own handler.
    pub fn handle_key(&mut self, key: DiffKey) -> DiffAction {
        if !matches!(self.modal, DiffModal::None) {
            // Outside the diff-view modal keymap, normal keys are
            // ignored; the App's modal-key handler is the right place
            // to route 'Y'/'N' / text-input. We expose dedicated
            // helpers (`commit_draft_*`, `confirm_discard`) for that
            // path.
            return DiffAction::None;
        }
        match key {
            DiffKey::Up => {
                self.move_up();
                DiffAction::None
            }
            DiffKey::Down => {
                self.move_down();
                DiffAction::None
            }
            DiffKey::ToggleFold => {
                if let Some(path) = self.toggle_expand() {
                    DiffAction::FetchDiff {
                        path,
                        staged: false,
                    }
                } else {
                    DiffAction::None
                }
            }
            DiffKey::CycleFocus => {
                self.cycle_focus();
                DiffAction::None
            }
            DiffKey::Stage => {
                if let Some((_, id)) = self.current_hunk_id() {
                    DiffAction::Stage { hunk_id: id }
                } else {
                    DiffAction::None
                }
            }
            DiffKey::Unstage => {
                if let Some((_, id)) = self.current_hunk_id() {
                    DiffAction::Unstage { hunk_id: id }
                } else {
                    DiffAction::None
                }
            }
            DiffKey::Discard => {
                self.request_discard_hunk();
                DiffAction::None
            }
            DiffKey::Commit => {
                self.open_commit_modal();
                DiffAction::None
            }
            DiffKey::OpenEditor => {
                if let Some(path) = self.current_file_path() {
                    DiffAction::OpenEditor { path, line: None }
                } else {
                    DiffAction::None
                }
            }
            DiffKey::Exit => DiffAction::Exit,
        }
    }
}

/// In-memory implementation of [`DiffSource`] for unit tests.
#[derive(Default, Clone)]
pub struct MockDiffSource {
    inner: std::sync::Arc<std::sync::Mutex<MockState>>,
}

#[derive(Default)]
struct MockState {
    files: Vec<FileEntry>,
    diffs: std::collections::HashMap<String, DiffPayload>,
    pub staged: Vec<String>,
    pub unstaged: Vec<String>,
    pub discarded: Vec<(Vec<String>, bool)>,
    pub commits: Vec<String>,
    pub editor_calls: Vec<String>,
}

impl MockDiffSource {
    pub fn with_files(files: Vec<FileEntry>) -> Self {
        let me = Self::default();
        me.inner.lock().unwrap().files = files;
        me
    }

    pub fn set_diff(&self, path: &str, payload: DiffPayload) {
        self.inner
            .lock()
            .unwrap()
            .diffs
            .insert(path.to_string(), payload);
    }

    pub fn staged(&self) -> Vec<String> {
        self.inner.lock().unwrap().staged.clone()
    }

    pub fn unstaged(&self) -> Vec<String> {
        self.inner.lock().unwrap().unstaged.clone()
    }

    pub fn discarded(&self) -> Vec<(Vec<String>, bool)> {
        self.inner.lock().unwrap().discarded.clone()
    }

    pub fn commits(&self) -> Vec<String> {
        self.inner.lock().unwrap().commits.clone()
    }

    pub fn editor_calls(&self) -> Vec<String> {
        self.inner.lock().unwrap().editor_calls.clone()
    }
}

impl DiffSource for MockDiffSource {
    fn snapshot(&self) -> Vec<FileEntry> {
        self.inner.lock().unwrap().files.clone()
    }

    fn diff_file(&self, path: &str, _staged: bool) -> Option<DiffPayload> {
        self.inner.lock().unwrap().diffs.get(path).cloned()
    }

    fn stage(&self, hunk_ids: &[String]) {
        self.inner
            .lock()
            .unwrap()
            .staged
            .extend(hunk_ids.iter().cloned());
    }

    fn unstage(&self, hunk_ids: &[String]) {
        self.inner
            .lock()
            .unwrap()
            .unstaged
            .extend(hunk_ids.iter().cloned());
    }

    fn discard(&self, hunk_ids: &[String], confirmed: bool) {
        self.inner
            .lock()
            .unwrap()
            .discarded
            .push((hunk_ids.to_vec(), confirmed));
    }

    fn commit(&self, message: &str) {
        self.inner.lock().unwrap().commits.push(message.to_string());
    }

    fn open_in_editor(&self, path: &str, _line: Option<u32>) {
        self.inner
            .lock()
            .unwrap()
            .editor_calls
            .push(path.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use la_proto::methods::{FileKind, FileStatus};

    fn entry(path: &str) -> FileEntry {
        FileEntry {
            path: path.to_string(),
            old_path: None,
            status: FileStatus::Modified,
            kind: FileKind::Text,
            staged_hunks: 0,
            unstaged_hunks: 1,
            size_bytes: 16,
            mode_change: None,
        }
    }

    fn hunk(id: &str) -> Hunk {
        Hunk {
            hunk_id: id.into(),
            staged: false,
            old_range: la_proto::methods::LineRange { start: 1, count: 1 },
            new_range: la_proto::methods::LineRange { start: 1, count: 1 },
            header: "@@ -1,1 +1,1 @@".into(),
            lines: vec![],
        }
    }

    #[test]
    fn apply_status_preserves_fold_state() {
        let mut v = DiffView::new();
        v.apply_status(vec![entry("a"), entry("b")]);
        v.toggle_expand(); // expand "a"
        v.apply_status(vec![entry("a"), entry("b")]);
        assert!(v.files[0].expanded);
    }

    #[test]
    fn discard_without_confirmation_does_not_fire() {
        let src = MockDiffSource::default();
        let mut v = DiffView::new();
        v.apply_status(vec![entry("a")]);
        v.apply_diff(DiffPayload {
            file: entry("a"),
            hunks: vec![hunk("h1")],
            truncated: None,
        });
        v.cycle_focus(); // → Hunks
        let action = v.handle_key(DiffKey::Discard);
        assert_eq!(action, DiffAction::None, "no RPC fires until confirmed");
        assert!(matches!(v.modal, DiffModal::ConfirmDiscard { .. }));
        let confirmed = v.confirm_discard().expect("confirm produces ids");
        src.discard(&confirmed.0, true);
        assert_eq!(src.discarded(), vec![(vec!["h1".into()], true)]);
    }

    #[test]
    fn cancel_modal_clears_state() {
        let mut v = DiffView::new();
        v.apply_status(vec![entry("a")]);
        v.apply_diff(DiffPayload {
            file: entry("a"),
            hunks: vec![hunk("h1")],
            truncated: None,
        });
        v.cycle_focus();
        v.handle_key(DiffKey::Discard);
        v.cancel_modal();
        assert!(matches!(v.modal, DiffModal::None));
    }

    #[test]
    fn stage_emits_action_with_hunk_id() {
        let mut v = DiffView::new();
        v.apply_status(vec![entry("a")]);
        v.apply_diff(DiffPayload {
            file: entry("a"),
            hunks: vec![hunk("h1")],
            truncated: None,
        });
        v.cycle_focus();
        let act = v.handle_key(DiffKey::Stage);
        assert_eq!(
            act,
            DiffAction::Stage {
                hunk_id: "h1".into()
            }
        );
    }

    #[test]
    fn toggle_fold_requests_fetch_only_on_expand() {
        let mut v = DiffView::new();
        v.apply_status(vec![entry("a")]);
        let act = v.handle_key(DiffKey::ToggleFold);
        assert_eq!(
            act,
            DiffAction::FetchDiff {
                path: "a".into(),
                staged: false,
            }
        );
        let act2 = v.handle_key(DiffKey::ToggleFold);
        assert_eq!(act2, DiffAction::None);
    }

    #[test]
    fn commit_modal_takes_message() {
        let mut v = DiffView::new();
        v.open_commit_modal();
        v.commit_draft_push('h');
        v.commit_draft_push('i');
        let msg = v.take_commit_message();
        assert_eq!(msg.as_deref(), Some("hi"));
        assert!(matches!(v.modal, DiffModal::None));
    }

    #[test]
    fn commit_modal_rejects_empty_draft() {
        let mut v = DiffView::new();
        v.open_commit_modal();
        let msg = v.take_commit_message();
        assert!(msg.is_none(), "empty draft keeps modal open");
        assert!(matches!(v.modal, DiffModal::Commit { .. }));
    }

    #[test]
    fn open_editor_emits_path() {
        let mut v = DiffView::new();
        v.apply_status(vec![entry("src/main.rs")]);
        let act = v.handle_key(DiffKey::OpenEditor);
        assert_eq!(
            act,
            DiffAction::OpenEditor {
                path: "src/main.rs".into(),
                line: None
            }
        );
    }
}
