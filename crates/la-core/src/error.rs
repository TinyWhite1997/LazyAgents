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
        }
    }
}

pub type CoreResult<T> = std::result::Result<T, CoreError>;
