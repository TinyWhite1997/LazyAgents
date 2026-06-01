//! Per-session output hub: multi-attach, backpressure, replay, reconnect.
//!
//! `OutputHub` is the daemon-side fan-out point between a single session's
//! PTY producer and N attached clients. It owns:
//!
//! - A **ring buffer** of recent `session.output` chunks (default 2 MiB,
//!   matching architecture §6.2 "ring buffer ... 默认 2 MiB / session"). Old
//!   chunks are evicted in FIFO order when the ring fills.
//! - A registry of **subscribers**. Each subscriber has a private 1 MiB
//!   per-client queue (architecture §3 关键不变量: 背压); when that queue
//!   would overflow, the hub coalesces the dropped chunks into a single
//!   `session.gap{from_seq, to_seq, dropped_bytes}` event that is delivered
//!   ahead of the next chunk that DOES fit. Other subscribers are unaffected
//!   — this is the "fair dispatch" property called out in WEK-16.
//! - A **parking** mechanism: on disconnect, the hub keeps a subscription's
//!   id and any queued events alive for 30 s. A reconnect that quotes the
//!   same `(SubId, last_seq)` resumes the stream without losing bytes still
//!   in the ring.
//!
//! The hub is intentionally transport-agnostic: callers pull `HubEvent`s
//! from a [`Subscription`] handle and decide themselves how to push them
//! onto a [`crate::SendHalf`] (or anything else implementing the IPC sink
//! contract). Keeping IO outside the hub keeps the unit-test surface small
//! and means the same hub backs UDS, Named Pipe, and an in-memory testing
//! mock without conditional compilation.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use la_proto::chunking::chunk_session_output;
use la_proto::notifications::{SessionGapParams, SessionOutputParams};
use tokio::sync::{mpsc, Mutex};

/// Default ring-buffer budget per session (bytes).
///
/// Architecture §6.2: "ring buffer（默认 2 MiB / session）+ 广播给 attached
/// client". The hub evicts oldest chunks first; a subscriber that wants
/// older data needs to call [`OutputHub::replay`] before they are gone.
pub const DEFAULT_RING_BYTES: usize = 2 * 1024 * 1024;

/// Default per-subscriber outbound queue cap (bytes).
///
/// Architecture §3 关键不变量: "客户端订阅默认有 1 MiB 缓冲". A subscriber
/// that exceeds this gets a `session.gap` notice and the chunks behind it
/// are dropped rather than allowed to grow without bound.
pub const DEFAULT_SUB_QUEUE_BYTES: usize = 1024 * 1024;

/// How long the hub holds a parked subscription alive before evicting it.
///
/// WEK-16 acceptance: "客户端断线 daemon 保留订阅 30 秒；重连以 seq 续传".
pub const DEFAULT_PARK_DURATION: Duration = Duration::from_secs(30);

/// Opaque subscriber handle. Lives for the lifetime of an attachment and,
/// while parked, is what a reconnecting client quotes to resume in place
/// rather than starting a fresh subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubId(u64);

impl SubId {
    /// Numeric form for logs / metrics. Not stable across daemon restarts.
    pub fn get(&self) -> u64 {
        self.0
    }
}

/// An event the hub hands to a subscriber. Mirrors the wire-level
/// `session.output` / `session.gap` notifications one-for-one so the writer
/// task can serialize without further branching.
#[derive(Debug, Clone)]
pub enum HubEvent {
    Output(Arc<SessionOutputParams>),
    Gap(SessionGapParams),
}

/// Returns the PTY-payload length of a chunk WITHOUT decoding the base64.
///
/// `chunk_session_output` always produces chunks whose decoded length is at
/// most `SESSION_OUTPUT_CHUNK_BYTES`, so a back-of-the-envelope reversal of
/// the base64 expansion is exact for the chunks the hub itself emits. For
/// hand-built chunks the worst case is a single extra padding byte, well
/// inside the queue accounting's tolerance.
fn decoded_len(p: &SessionOutputParams) -> usize {
    let b64_len = p.data_base64.len();
    if b64_len == 0 {
        return 0;
    }
    let pad = p
        .data_base64
        .as_bytes()
        .iter()
        .rev()
        .take_while(|&&c| c == b'=')
        .count();
    (b64_len / 4) * 3 - pad
}

/// Hub configuration. Public so tests can shrink the ring / queue caps.
#[derive(Debug, Clone, Copy)]
pub struct HubConfig {
    pub ring_bytes: usize,
    pub sub_queue_bytes: usize,
    pub park_duration: Duration,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            ring_bytes: DEFAULT_RING_BYTES,
            sub_queue_bytes: DEFAULT_SUB_QUEUE_BYTES,
            park_duration: DEFAULT_PARK_DURATION,
        }
    }
}

/// Per-session output hub. Cheap to clone — internally `Arc<Inner>` so all
/// clones share state. Producer (`publish`) and consumers (`Subscription`)
/// can both hold their own clone.
#[derive(Clone)]
pub struct OutputHub {
    inner: Arc<Inner>,
    config: HubConfig,
}

struct Inner {
    session_id: String,
    state: Mutex<HubState>,
}

struct HubState {
    next_seq: u64,
    /// FIFO ring of recent chunks. Front is oldest. Sized by `ring_bytes` of
    /// decoded payload, not by chunk count.
    ring: VecDeque<Arc<SessionOutputParams>>,
    ring_bytes: usize,
    /// Active + parked subscribers.
    subs: HashMap<SubId, SubEntry>,
    next_sub_id: u64,
}

struct SubEntry {
    /// Send half of the per-sub unbounded channel. Unbounded because the
    /// queue budget is enforced by `queued_bytes` below, not by channel
    /// backpressure — overflow becomes a `Gap` event, not a producer block.
    /// `None` after eviction.
    tx: Option<mpsc::UnboundedSender<HubEvent>>,
    /// Decoded-bytes charge across pending events (only `Output` events
    /// count). Updated by the producer on push and by [`Subscription::recv`]
    /// on consume.
    queued_bytes: usize,
    /// In-flight coalesced gap. Held while the queue is at or above the cap
    /// and we are dropping chunks; flushed onto the queue alongside the
    /// first dropped chunk and updated in place as more drop. Reset when
    /// the subscriber catches up enough to enqueue a real chunk.
    pending_gap: Option<PendingGap>,
    /// True after the hub has closed this subscription (shutdown or
    /// eviction). Producer drops the sender; consumer's recv yields `None`
    /// once the channel drains.
    closed: bool,
    /// Park deadline: set when the [`Subscription`] handle is dropped.
    /// Cleared on `resume`.
    parked: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
struct PendingGap {
    from_seq: u64,
    to_seq: u64,
    dropped_bytes: u64,
}

impl OutputHub {
    /// Build a new hub for a session, using the default budgets.
    pub fn new(session_id: impl Into<String>) -> Self {
        Self::with_config(session_id, HubConfig::default())
    }

    /// Build a new hub with custom budgets. Intended for tests; production
    /// callers should use [`OutputHub::new`].
    pub fn with_config(session_id: impl Into<String>, config: HubConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                session_id: session_id.into(),
                state: Mutex::new(HubState {
                    next_seq: 1,
                    ring: VecDeque::new(),
                    ring_bytes: 0,
                    subs: HashMap::new(),
                    next_sub_id: 1,
                }),
            }),
            config,
        }
    }

    /// Session id this hub belongs to.
    pub fn session_id(&self) -> &str {
        &self.inner.session_id
    }

    /// Next sequence number the hub will assign on `publish`.
    pub async fn next_seq(&self) -> u64 {
        self.inner.state.lock().await.next_seq
    }

    /// Publish a raw PTY payload. The hub chunks it via
    /// [`chunk_session_output`], assigns the next seq, appends to the ring,
    /// and fans out to every active subscriber.
    ///
    /// Returns the inclusive (`first_seq`, `last_seq`) range of chunks
    /// emitted; useful for logging and for the test harness to assert
    /// dispatch ordering. An empty payload still produces one heartbeat
    /// chunk (per [`chunk_session_output`] semantics).
    pub async fn publish(&self, payload: &[u8]) -> (u64, u64) {
        let mut state = self.inner.state.lock().await;
        let start = state.next_seq;
        let chunks = chunk_session_output(&self.inner.session_id, start, payload);
        let mut last = start;
        for c in chunks {
            last = c.seq;
            let arc = Arc::new(c);
            let dec_len = decoded_len(&arc);
            // Append to ring, evict as needed.
            state.ring.push_back(arc.clone());
            state.ring_bytes += dec_len;
            while state.ring_bytes > self.config.ring_bytes {
                let evicted = state.ring.pop_front();
                match evicted {
                    Some(e) => state.ring_bytes = state.ring_bytes.saturating_sub(decoded_len(&e)),
                    None => break,
                }
            }
            // Fan out to active subs.
            let cap = self.config.sub_queue_bytes;
            for entry in state.subs.values_mut() {
                if entry.closed || entry.parked.is_some() {
                    continue;
                }
                push_to_sub(entry, HubEvent::Output(arc.clone()), dec_len, cap);
            }
            state.next_seq = arc.seq + 1;
        }
        (start, last)
    }

    /// Create a new subscription. The subscriber's queue is primed by
    /// replaying any chunks in the ring whose `seq` is strictly greater
    /// than `since_seq`. Pass `None` to start fresh (no replay); pass
    /// `Some(snapshot_seq)` to skip what the subscriber has already seen.
    ///
    /// The caller spawns a writer task that loops on
    /// [`Subscription::recv`] until it returns `None`.
    pub async fn subscribe(&self, since_seq: Option<u64>) -> Subscription {
        let mut state = self.inner.state.lock().await;
        let id = SubId(state.next_sub_id);
        state.next_sub_id += 1;
        let (tx, rx) = mpsc::unbounded_channel();
        let mut entry = SubEntry {
            tx: Some(tx),
            queued_bytes: 0,
            pending_gap: None,
            closed: false,
            parked: None,
        };
        // Replay anything fresh enough.
        if let Some(since) = since_seq {
            let cap = self.config.sub_queue_bytes;
            for c in &state.ring {
                if c.seq > since {
                    let dec_len = decoded_len(c);
                    push_to_sub(&mut entry, HubEvent::Output(c.clone()), dec_len, cap);
                }
            }
        }
        state.subs.insert(id, entry);
        Subscription {
            id,
            hub: self.clone(),
            rx,
        }
    }

    /// Resume a previously-parked subscription, draining any chunks queued
    /// while the writer was away. Returns `None` if the id is unknown
    /// (already evicted past the park deadline), in which case the caller
    /// should fall back to a fresh [`subscribe`](Self::subscribe).
    ///
    /// `since_seq` causes the hub to replay any ring chunks newer than that
    /// seq which are not already queued (parking keeps the queue intact;
    /// we deduplicate against the ring). The resumed [`Subscription`] owns
    /// a fresh `mpsc::Receiver` — the parked subscription's receiver was
    /// dropped along with the original handle, so any in-flight events
    /// have been re-pushed onto the new channel before this returns.
    pub async fn resume(&self, id: SubId, since_seq: Option<u64>) -> Option<Subscription> {
        let mut state = self.inner.state.lock().await;
        let entry = state.subs.get_mut(&id)?;
        if entry.closed {
            return None;
        }
        // The old Receiver was dropped when the previous Subscription was
        // dropped. Create a new channel and re-attach. Anything still
        // pending against `queued_bytes` is the gap accounting + the chunks
        // that piled up while parked; the latter live only in `state.ring`
        // (which we'll replay), so we reset the per-sub queue here.
        let (tx, rx) = mpsc::unbounded_channel();
        entry.tx = Some(tx);
        entry.queued_bytes = 0;
        entry.pending_gap = None;
        entry.parked = None;
        // Replay only fresh chunks: anything seq > since_seq still in the
        // ring. Without a since_seq, the caller is opting out of catch-up.
        if let Some(since) = since_seq {
            let cap = self.config.sub_queue_bytes;
            let to_replay: Vec<Arc<SessionOutputParams>> = state
                .ring
                .iter()
                .filter(|c| c.seq > since)
                .cloned()
                .collect();
            let entry = state.subs.get_mut(&id).expect("entry still present");
            for c in to_replay {
                let dec_len = decoded_len(&c);
                push_to_sub(entry, HubEvent::Output(c), dec_len, cap);
            }
        }
        Some(Subscription {
            id,
            hub: self.clone(),
            rx,
        })
    }

    /// Synchronously park a subscription: mark it as disconnected, start
    /// the eviction timer, but keep its queue + id alive for at most
    /// [`HubConfig::park_duration`] so a fast reconnect doesn't lose any
    /// bytes still in flight.
    ///
    /// The caller schedules a [`tokio::spawn`] that calls
    /// [`evict_if_still_parked`](Self::evict_if_still_parked) after the
    /// park duration; the hub itself doesn't spawn tasks so a daemon under
    /// shutdown isn't fighting orphaned timers.
    pub async fn park(&self, id: SubId) {
        let mut state = self.inner.state.lock().await;
        if let Some(entry) = state.subs.get_mut(&id) {
            if entry.parked.is_none() && !entry.closed {
                entry.parked = Some(Instant::now() + self.config.park_duration);
                // Drop the sender so any straggler that picks up a clone
                // won't push into a dead channel. The Receiver is owned
                // by the dropped Subscription, so events queued while
                // parked land in the channel buffer and are dropped at
                // resume time (we resync via the ring replay path).
                entry.tx = None;
            }
        }
    }

    /// Evict the subscription if it is still parked past its deadline.
    /// Called by the per-sub park timer; safe to call after a successful
    /// resume (it will be a no-op because `parked` was cleared).
    pub async fn evict_if_still_parked(&self, id: SubId) -> bool {
        let mut state = self.inner.state.lock().await;
        let Some(entry) = state.subs.get(&id) else {
            return false;
        };
        let should_evict = match entry.parked {
            Some(deadline) => Instant::now() >= deadline,
            None => false,
        };
        if should_evict {
            state.subs.remove(&id);
            true
        } else {
            false
        }
    }

    /// Close the hub for shutdown: every subscriber's next `recv` returns
    /// `None`. The ring buffer is dropped. Future `publish` calls become
    /// no-ops on the now-empty subscriber list.
    pub async fn close(&self) {
        let mut state = self.inner.state.lock().await;
        // Dropping each entry drops its sender; any pending recv() yields
        // None as the channel closes.
        state.subs.clear();
        state.ring.clear();
        state.ring_bytes = 0;
    }

    /// Replay chunks from the ring buffer starting at (and including)
    /// `from_seq`, up to `max_bytes` of decoded payload. Returns the
    /// chunks actually still in the ring (which may be a strict suffix of
    /// the requested range if eviction has caught up).
    ///
    /// The hub does NOT enqueue these to any subscription — that is the
    /// caller's job, because `sessions.replay` is an explicit RPC and
    /// shouldn't be re-driven on a generic reattach.
    pub async fn replay(&self, from_seq: u64, max_bytes: u64) -> Vec<Arc<SessionOutputParams>> {
        let state = self.inner.state.lock().await;
        let mut out = Vec::new();
        let mut budget = max_bytes;
        for c in &state.ring {
            if c.seq < from_seq {
                continue;
            }
            let dec_len = decoded_len(c) as u64;
            if budget == 0 {
                break;
            }
            out.push(c.clone());
            budget = budget.saturating_sub(dec_len);
        }
        out
    }

    /// Snapshot the current oldest / newest seq in the ring. `None` when
    /// the ring is empty. Used by `sessions.attach` to decide whether a
    /// requested `since_seq` is replayable or only partly so.
    pub async fn ring_range(&self) -> Option<(u64, u64)> {
        let state = self.inner.state.lock().await;
        match (state.ring.front(), state.ring.back()) {
            (Some(f), Some(b)) => Some((f.seq, b.seq)),
            _ => None,
        }
    }

    /// Test-only / introspection: number of currently-tracked subscriptions
    /// (active + parked).
    pub async fn sub_count(&self) -> usize {
        self.inner.state.lock().await.subs.len()
    }

    /// Decrement the per-sub queued-bytes accounting when a subscriber
    /// consumes an event. Called by [`Subscription::recv`].
    async fn ack(&self, id: SubId, bytes: usize) {
        let mut state = self.inner.state.lock().await;
        if let Some(entry) = state.subs.get_mut(&id) {
            entry.queued_bytes = entry.queued_bytes.saturating_sub(bytes);
        }
    }
}

/// Try to push an event to a single subscriber's queue, charging
/// `event_bytes` against its queue budget.
///
/// When the queue would overflow, the chunk is dropped and the subscriber
/// gets a `Gap` notice instead. The gap notice itself is NOT charged to
/// the queue budget (it is tiny by design and represents data we already
/// refused to deliver), so it is always deliverable. One Gap is emitted
/// per dropped chunk so the subscriber can sum `dropped_bytes` to know
/// exactly how many bytes are missing; a consumer that prefers a single
/// "you have a gap here" pulse can coalesce in user-space using `from_seq`
/// and `to_seq` (they remain disjoint per emission and contiguous across
/// a burst).
fn push_to_sub(entry: &mut SubEntry, event: HubEvent, event_bytes: usize, cap: usize) {
    let Some(tx) = entry.tx.as_ref() else {
        return;
    };
    let would_overflow = entry.queued_bytes + event_bytes > cap;
    if !would_overflow {
        entry.pending_gap = None;
        entry.queued_bytes += event_bytes;
        let _ = tx.send(event);
        return;
    }
    let HubEvent::Output(p) = event else {
        return;
    };
    // Track the cumulative gap state so callers that want it can build a
    // single user-facing notice. We still emit one Gap per dropped chunk
    // (each describing JUST that chunk) so `dropped_bytes` arithmetic on
    // the receive side is straightforward — summing N gaps yields the
    // true byte count, which a coalesced gap couldn't express without
    // mutability on the wire.
    if let Some(g) = entry.pending_gap.as_mut() {
        if p.seq > g.to_seq {
            g.to_seq = p.seq;
        }
        if p.seq < g.from_seq {
            g.from_seq = p.seq;
        }
        g.dropped_bytes += event_bytes as u64;
    } else {
        entry.pending_gap = Some(PendingGap {
            from_seq: p.seq,
            to_seq: p.seq,
            dropped_bytes: event_bytes as u64,
        });
    }
    let _ = tx.send(HubEvent::Gap(SessionGapParams {
        session_id: p.session_id.clone(),
        from_seq: p.seq,
        to_seq: p.seq,
        dropped_bytes: event_bytes as u64,
    }));
}

/// Subscription handle. Pulls events with `recv` and yields its slot back
/// to the hub on drop (parking the subscription with the configured grace).
///
/// Cloning the underlying [`OutputHub`] separately is fine — this handle
/// represents one writer task, not the subscription state itself, which
/// lives in the hub.
pub struct Subscription {
    id: SubId,
    hub: OutputHub,
    rx: mpsc::UnboundedReceiver<HubEvent>,
}

impl Subscription {
    pub fn id(&self) -> SubId {
        self.id
    }

    pub fn hub(&self) -> &OutputHub {
        &self.hub
    }

    /// Synchronously park this subscription and consume it. Preferred over
    /// relying on [`Drop`] when the caller has an `async` context (e.g. the
    /// daemon's connection reader observing EOF) — it guarantees the parked
    /// state is visible *before* the caller schedules its eviction timer,
    /// avoiding a reconnect-vs-park race in which a fast resume() finds the
    /// entry not-yet-parked, or the timer fires before park lands.
    ///
    /// Callers that drop the [`Subscription`] without calling this still get
    /// best-effort parking via the [`Drop`] impl (a detached `tokio::spawn`),
    /// which is fine for shutdown paths but introduces nondeterminism into
    /// any timer-driven eviction logic that runs immediately after.
    pub async fn park(self) {
        self.hub.park(self.id).await;
    }

    /// Pull the next event for this subscriber, awaiting until one arrives
    /// or the hub closes the subscription. Returns `None` when the channel
    /// is closed and drained (which happens on hub close, eviction past the
    /// park deadline, or — racing against `resume` — when the producer
    /// drops the old sender to swap in a new one).
    pub async fn recv(&mut self) -> Option<HubEvent> {
        let ev = self.rx.recv().await?;
        // Update the per-sub byte charge. Doing this under recv() rather
        // than from the consumer's side keeps the accounting bounded — a
        // misbehaving writer that takes a long time to push to the network
        // won't keep the queue charge inflated, because as far as the hub
        // is concerned the chunk is already consumed once it leaves the
        // channel.
        if let HubEvent::Output(p) = &ev {
            self.hub.ack(self.id, decoded_len(p)).await;
        }
        Some(ev)
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // Best-effort parking for callers that didn't go through the
        // preferred [`Subscription::park`] async path (e.g. abrupt panic on
        // the writer task, tests that just `drop()`). The detached spawn
        // means there is a brief window during which a fast reconnect could
        // observe `parked.is_none()` and fall through; production paths
        // should call `.park().await` explicitly when they observe EOF.
        // Eviction is the caller's responsibility — see `evict_if_still_parked`.
        let hub = self.hub.clone();
        let id = self.id;
        tokio::spawn(async move {
            hub.park(id).await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small_config() -> HubConfig {
        HubConfig {
            ring_bytes: 4 * 1024,
            sub_queue_bytes: 2 * 1024,
            park_duration: Duration::from_millis(100),
        }
    }

    #[tokio::test]
    async fn publish_chunks_and_assigns_monotonic_seq() {
        let hub = OutputHub::new("sid");
        let (start, last) = hub.publish(b"hello").await;
        assert_eq!(start, 1);
        assert_eq!(last, 1);
        let (start2, last2) = hub.publish(b"world").await;
        assert_eq!(start2, 2);
        assert_eq!(last2, 2);
        assert_eq!(hub.next_seq().await, 3);
    }

    #[tokio::test]
    async fn subscriber_sees_chunks_in_order() {
        let hub = OutputHub::new("sid");
        let mut sub = hub.subscribe(None).await;

        let h = {
            let hub = hub.clone();
            tokio::spawn(async move {
                hub.publish(b"one").await;
                hub.publish(b"two").await;
            })
        };
        h.await.unwrap();

        let mut out = Vec::new();
        for _ in 0..2 {
            let ev = sub.recv().await.unwrap();
            let HubEvent::Output(p) = ev else { panic!() };
            out.push((p.seq, p.data_bytes().unwrap()));
        }
        assert_eq!(out[0], (1, b"one".to_vec()));
        assert_eq!(out[1], (2, b"two".to_vec()));
    }

    #[tokio::test]
    async fn slow_subscriber_gets_gap_but_others_drain_fine() {
        let cfg = small_config();
        let hub = OutputHub::with_config("sid", cfg);

        let mut slow = hub.subscribe(None).await;
        let mut fast = hub.subscribe(None).await;

        // Concurrently drain the fast subscriber so its queue never fills.
        // The slow subscriber doesn't read, so its 2 KiB queue overflows
        // after the first couple of 1 KiB chunks.
        let fast_handle = tokio::spawn(async move {
            let mut count = 0;
            let mut gap_seen = false;
            while count < 5 {
                match tokio::time::timeout(Duration::from_secs(2), fast.recv()).await {
                    Ok(Some(HubEvent::Output(_))) => count += 1,
                    Ok(Some(HubEvent::Gap(_))) => gap_seen = true,
                    _ => break,
                }
            }
            (count, gap_seen)
        });

        let payload = vec![b'x'; 1024];
        for _ in 0..5 {
            hub.publish(&payload).await;
            tokio::task::yield_now().await;
        }

        let (fast_count, fast_gap) = fast_handle.await.unwrap();
        assert_eq!(fast_count, 5, "fast subscriber must receive every chunk");
        assert!(!fast_gap, "fast subscriber must not observe a gap");

        let mut slow_outputs = 0u64;
        let mut slow_gap_dropped = 0u64;
        loop {
            match tokio::time::timeout(Duration::from_millis(50), slow.recv()).await {
                Ok(Some(HubEvent::Output(_))) => slow_outputs += 1,
                Ok(Some(HubEvent::Gap(g))) => slow_gap_dropped += g.dropped_bytes,
                _ => break,
            }
        }
        assert!(
            slow_gap_dropped > 0,
            "slow subscriber must observe a gap notice (got {slow_outputs} outputs, {slow_gap_dropped} dropped bytes)"
        );
        assert_eq!(
            slow_outputs + slow_gap_dropped / 1024,
            5,
            "outputs ({slow_outputs}) + dropped chunks ({}) must equal 5",
            slow_gap_dropped / 1024
        );
    }

    #[tokio::test]
    async fn park_then_resume_replays_missed_bytes() {
        let hub = OutputHub::new("sid");
        let mut sub = hub.subscribe(None).await;

        hub.publish(b"first").await;
        let ev = sub.recv().await.unwrap();
        let last_seq = if let HubEvent::Output(p) = ev {
            p.seq
        } else {
            panic!()
        };

        let id = sub.id();
        drop(sub);
        tokio::task::yield_now().await;
        for _ in 0..10 {
            if hub
                .inner
                .state
                .lock()
                .await
                .subs
                .get(&id)
                .and_then(|e| e.parked)
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        hub.publish(b"second").await;
        hub.publish(b"third").await;

        let mut sub = hub.resume(id, Some(last_seq)).await.expect("resume");
        let mut got = Vec::new();
        for _ in 0..2 {
            let ev = sub.recv().await.unwrap();
            let HubEvent::Output(p) = ev else { panic!() };
            got.push(p.data_bytes().unwrap());
        }
        assert_eq!(got[0], b"second");
        assert_eq!(got[1], b"third");
    }

    #[tokio::test]
    async fn explicit_park_is_visible_synchronously() {
        // Daemon-shaped pattern: when the connection reader observes EOF,
        // it should call sub.park().await so the parked state is visible
        // to a concurrent resume() before the caller arms its eviction
        // timer. The Drop-only path is best-effort and may race; this is
        // the deterministic path callers should use.
        let hub = OutputHub::new("sid");
        let sub = hub.subscribe(None).await;
        let id = sub.id();
        sub.park().await;
        let parked = hub
            .inner
            .state
            .lock()
            .await
            .subs
            .get(&id)
            .and_then(|e| e.parked)
            .is_some();
        assert!(
            parked,
            "park().await must mark the sub as parked before returning"
        );
    }

    #[tokio::test]
    async fn eviction_after_park_deadline_drops_subscription() {
        let cfg = HubConfig {
            park_duration: Duration::from_millis(20),
            ..HubConfig::default()
        };
        let hub = OutputHub::with_config("sid", cfg);
        let sub = hub.subscribe(None).await;
        let id = sub.id();
        drop(sub);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(hub.evict_if_still_parked(id).await);
        assert_eq!(hub.sub_count().await, 0);
        assert!(hub.resume(id, None).await.is_none());
    }

    #[tokio::test]
    async fn ring_evicts_oldest_when_full() {
        let cfg = HubConfig {
            ring_bytes: 1024,
            ..HubConfig::default()
        };
        let hub = OutputHub::with_config("sid", cfg);
        let payload = vec![b'x'; 512];
        for _ in 0..4 {
            hub.publish(&payload).await;
        }
        let (front, back) = hub.ring_range().await.unwrap();
        assert!(
            back - front <= 1,
            "ring expected to evict; got [{front}, {back}]"
        );
    }

    #[tokio::test]
    async fn replay_returns_only_chunks_still_in_ring() {
        let cfg = HubConfig {
            ring_bytes: 1024,
            ..HubConfig::default()
        };
        let hub = OutputHub::with_config("sid", cfg);
        let payload = vec![b'x'; 512];
        for _ in 0..4 {
            hub.publish(&payload).await;
        }
        let chunks = hub.replay(1, 10_000).await;
        assert!(!chunks.is_empty());
        for c in &chunks {
            assert!(c.seq >= 3, "evicted chunk reappeared: seq={}", c.seq);
        }
    }

    #[tokio::test]
    async fn since_seq_skips_already_seen_chunks_on_subscribe() {
        let hub = OutputHub::new("sid");
        hub.publish(b"alpha").await;
        hub.publish(b"beta").await;
        let mut sub = hub.subscribe(Some(1)).await;
        let ev = sub.recv().await.unwrap();
        let HubEvent::Output(p) = ev else { panic!() };
        assert_eq!(p.seq, 2);
        assert_eq!(p.data_bytes().unwrap(), b"beta");
    }

    #[tokio::test]
    async fn close_wakes_pending_receivers() {
        let hub = OutputHub::new("sid");
        let mut sub = hub.subscribe(None).await;
        let h = tokio::spawn(async move { sub.recv().await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        hub.close().await;
        let res = tokio::time::timeout(Duration::from_millis(100), h)
            .await
            .expect("recv hung after close")
            .unwrap();
        assert!(res.is_none(), "recv should yield None after close");
    }
}
