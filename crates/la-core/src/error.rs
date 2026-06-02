use la_adapter::AdapterError;
use la_ipc::IpcError;
use la_pty::PtyError;
use la_storage::StorageError;

/// All errors `la-core` surfaces to the IPC dispatcher.
///
/// The variants double as the input to `la_proto::to_rpc_error` — there is
/// a 1:1 mapping from each business variant to a [`la_proto::ErrorKind`],
/// pinned by the `error_mapping` unit test. New variants MUST update both.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// Adapter / backend layer reported a failure (not installed, drift, …).
    /// The wrapped [`AdapterError`] keeps the classification so the
    /// dispatcher can map it to the right `-33100…` business code.
    #[error("adapter: {0}")]
    Adapter(#[from] AdapterError),

    /// PTY spawn / IO failure (the read or write loop died, signal map
    /// rejected the pid, openpty failed). Mapped to `AdapterSpawnFailed`
    /// when raised from `spawn_session`, otherwise `Internal`.
    #[error("pty: {0}")]
    Pty(#[from] PtyError),

    /// Storage (SQLite) reported an error. Wrapped so the dispatcher can
    /// distinguish `StorageBusy` / `StorageConflict` / `StorageFailed`
    /// by inspecting the inner.
    #[error("storage: {0}")]
    Storage(#[from] StorageError),

    /// IPC hub error (currently only surfaces if we ever try to publish to
    /// a closed hub; kept for completeness so the manager doesn't have to
    /// invent a new variant).
    #[error("ipc: {0}")]
    Ipc(#[from] IpcError),

    /// `session_id` does not exist in the manager's registry.
    #[error("session not found: {0}")]
    SessionNotFound(String),

    /// Caller tried `sessions.write` while another client owns the input.
    #[error("writer locked by sub {holder}")]
    WriterLocked { holder: u64 },

    /// Caller asked for a state change that requires the session to be
    /// attached first.
    #[error("session not attached")]
    NotAttached,

    /// Hard-delete / archive attempted while the session is still running.
    #[error("session still running")]
    SessionBusy,

    /// Generic catch-all. Wrap a `String` only when none of the typed
    /// variants fit (e.g. an `Other` IO error during orphan scan).
    #[error("internal: {0}")]
    Internal(String),

    // ---------- Worktree provisioning (M2 / WEK-27) ----------
    //
    // The session manager calls `WorktreeManager::create` when
    // `SessionsCreateParams.worktree = true`. Every typed failure that
    // can stop a worktree from being provisioned is its own variant so
    // the dispatcher can map it to the right `-33110..-33119` code and
    // the TUI can show an actionable hint without parsing strings.
    /// `project_dir` is not (inside) a git repository.
    #[error("not a git repo: {path}")]
    NotAGitRepo { path: String },
    /// `git worktree add` refused because the worktree slot is busy —
    /// dirty index, lock file held, sibling worktree already checked out
    /// at the requested branch.
    #[error("worktree busy: {reason}")]
    WorktreeBusy { reason: String },
    /// `la/session-<sid>` branch already exists. Should be impossible
    /// under normal sid generation; treat as state damage.
    #[error("branch already exists: {branch}")]
    BranchCollision { branch: String },
    /// Filesystem failure (ENOSPC, EACCES, …) while creating or removing
    /// the worktree directory.
    #[error("worktree io: {0}")]
    WorktreeIo(#[source] std::io::Error),
    /// `git` binary missing or too old to support `worktree add -b`
    /// (< 2.20). `hint` carries the install / upgrade advice.
    #[error("git unavailable: {hint}")]
    GitUnavailable { hint: String },
    /// Catch-all for `git worktree add` failures that don't match the
    /// typed patterns. `stderr` is the trimmed first 4 KiB of git's
    /// stderr so the user can self-diagnose without daemon log access.
    #[error("worktree provision failed: {stderr}")]
    WorktreeProvision { stderr: String },

    // ---------- Worktree diff review (M2.5 / WEK-28) ----------
    //
    // Failures from the seven `worktree.*` diff RPCs. Each one maps
    // to a `-33120..-33127` JSON-RPC code via [`Self::kind`]. New
    // variants must update both ends.
    /// Session has no `worktree_path` recorded (or the session id
    /// doesn't exist). Returned by every `worktree.*` method before
    /// any git invocation.
    #[error("worktree unavailable for session {session_id}")]
    WorktreeUnavailable { session_id: String },
    /// `worktree.diff` was asked for a file that exceeds the inline
    /// cap. The dispatcher should return a `TruncationMarker` instead
    /// of raising — this variant is reserved for future
    /// "force_full = true" overrides.
    #[error("diff too large: {size_bytes} bytes")]
    WorktreeDiffTooLarge { size_bytes: u64 },
    /// Every `hunk_id` in a stage / unstage / discard request is
    /// stale. Partial staleness goes in the result's `rejected` array
    /// instead.
    #[error("all hunk ids are stale")]
    WorktreeHunkStale,
    /// A pre-commit hook returned non-zero. `stderr` is the trimmed
    /// hook output.
    #[error("commit hook failed: {stderr}")]
    WorktreeCommitHookFailed { stderr: String },
    /// `git commit` reported "nothing to commit" without
    /// `--allow-empty`.
    #[error("nothing to commit")]
    WorktreeCommitEmpty,
    /// `git apply --cached` (or `--reverse`) rejected the synthesised
    /// patch — typically index drift between the diff read and the
    /// apply. `stderr` carries git's reason.
    #[error("patch rejected: {stderr}")]
    WorktreePatchRejected { stderr: String },
    /// `worktree.open_in_editor` could not resolve any editor (no
    /// override, no `$VISUAL`, no `$EDITOR`, no `code` on `$PATH`).
    #[error("no editor configured; set $VISUAL or $EDITOR")]
    WorktreeEditorUnavailable,
    /// `worktree.discard` called without `confirmed: true`. The TUI
    /// must go through the 二次确认 modal before re-issuing.
    #[error("discard requires confirmed: true")]
    WorktreeDiscardUnconfirmed,
}

impl CoreError {
    /// Wire-error kind this variant maps to. The dispatcher uses this with
    /// `la_proto::to_rpc_error` to build the JSON-RPC error response.
    pub fn kind(&self) -> la_proto::ErrorKind {
        use la_proto::ErrorKind as K;
        match self {
            CoreError::Adapter(AdapterError::NotInstalled { .. }) => K::AdapterNotInstalled,
            CoreError::Adapter(AdapterError::Unauthenticated { .. }) => K::AdapterUnauthenticated,
            CoreError::Adapter(AdapterError::SpawnFailed(_)) => K::AdapterSpawnFailed,
            CoreError::Adapter(AdapterError::UnsupportedOption { .. }) => {
                K::AdapterUnsupportedOption
            }
            CoreError::Adapter(AdapterError::ProtocolDrift { .. }) => K::AdapterProtocolDrift,
            CoreError::Adapter(AdapterError::Transient(_)) => K::Internal,
            CoreError::Pty(_) => K::AdapterSpawnFailed,
            CoreError::Storage(StorageError::Busy { .. }) => K::StorageBusy,
            CoreError::Storage(StorageError::MissingSession(_)) => K::SessionNotFound,
            CoreError::Storage(StorageError::MissingProject(_)) => K::Internal,
            CoreError::Storage(_) => K::StorageFailed,
            CoreError::Ipc(_) => K::Internal,
            CoreError::SessionNotFound(_) => K::SessionNotFound,
            CoreError::WriterLocked { .. } => K::WriterLocked,
            CoreError::NotAttached => K::NotAttached,
            CoreError::SessionBusy => K::SessionBusy,
            CoreError::Internal(_) => K::Internal,
            CoreError::NotAGitRepo { .. } => K::WorktreeNotAGitRepo,
            CoreError::WorktreeBusy { .. } => K::WorktreeBusy,
            CoreError::BranchCollision { .. } => K::WorktreeBranchCollision,
            CoreError::WorktreeIo(_) => K::WorktreeIo,
            CoreError::GitUnavailable { .. } => K::WorktreeGitUnavailable,
            CoreError::WorktreeProvision { .. } => K::WorktreeProvisionFailed,
            CoreError::WorktreeUnavailable { .. } => K::WorktreeUnavailable,
            CoreError::WorktreeDiffTooLarge { .. } => K::WorktreeDiffTooLarge,
            CoreError::WorktreeHunkStale => K::WorktreeHunkStale,
            CoreError::WorktreeCommitHookFailed { .. } => K::WorktreeCommitHookFailed,
            CoreError::WorktreeCommitEmpty => K::WorktreeCommitEmpty,
            CoreError::WorktreePatchRejected { .. } => K::WorktreePatchRejected,
            CoreError::WorktreeEditorUnavailable => K::WorktreeEditorUnavailable,
            CoreError::WorktreeDiscardUnconfirmed => K::WorktreeDiscardUnconfirmed,
        }
    }
}

pub type CoreResult<T> = std::result::Result<T, CoreError>;
