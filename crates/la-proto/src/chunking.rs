//! Helpers for splitting a single PTY payload into multiple
//! `session.output` notifications.
//!
//! Architecture §3 requires that any single `session.output` carry at most
//! 64 KiB of decoded PTY bytes; longer bursts must be sliced. Sequence
//! numbers must remain monotonic across the resulting chunks.

use crate::notifications::SessionOutputParams;
use crate::SESSION_OUTPUT_CHUNK_BYTES;

/// Split `data` into `SessionOutputParams` whose decoded byte length is at
/// most [`SESSION_OUTPUT_CHUNK_BYTES`].
///
/// `start_seq` is the sequence number for the first chunk; subsequent chunks
/// increment by one. The returned value's length is the number of `seq`
/// numbers consumed, which the caller should use to advance its counter.
///
/// Empty input still emits a single chunk so that consumers observe a heartbeat
/// at the assigned `seq` rather than silently skipping a number — this keeps
/// the gap detector in [`crate::notifications::SessionOutput`] honest.
pub fn chunk_session_output(
    session_id: &str,
    start_seq: u64,
    data: &[u8],
) -> Vec<SessionOutputParams> {
    if data.is_empty() {
        return vec![SessionOutputParams::from_bytes(session_id, start_seq, &[])];
    }

    let mut out = Vec::with_capacity((data.len() + SESSION_OUTPUT_CHUNK_BYTES - 1) / SESSION_OUTPUT_CHUNK_BYTES);
    let mut seq = start_seq;
    for slice in data.chunks(SESSION_OUTPUT_CHUNK_BYTES) {
        out.push(SessionOutputParams::from_bytes(session_id, seq, slice));
        seq += 1;
    }
    out
}
