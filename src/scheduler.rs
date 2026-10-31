use crate::metadata::{FetchedMetadata, MetadataFetchOutcome, RbitFetcher};
use crate::peer_lookup::PeerLookupRequest;
#[cfg(test)]
use crate::runtime_stats::DhtRuntimeLimits;
use crate::runtime_stats::DhtRuntimeStats;
use crate::server::{HashDiscovered, HashSource};
use crate::types::{
    MetadataFetchCompletion, MetadataFetchCompletionStatus, MetadataOptions, NodeTuple, TorrentInfo,
};
use arc_swap::ArcSwapOption;
#[cfg(feature = "metrics")]
use metrics::{counter, gauge, histogram};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::net::SocketAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// Maximum actual Peer download attempts for one InfoHash, excluding cache skips.
pub const MAX_METADATA_PEERS_PER_HASH: usize = 12;
/// Bounded candidate scan/refill budget; cache skips must not consume download attempts.
pub(crate) const MAX_METADATA_CANDIDATES_PER_HASH: usize = 64;
const HASH_QUEUE_TTL: Duration = Duration::from_secs(60);
const PEER_COALESCE_WINDOW: Duration = Duration::from_millis(25);
const LIVE_PEER_IDLE_GRACE: Duration = Duration::from_millis(300);
const ACTIVE_PEER_LOOKUP_DELAY: Duration = Duration::from_millis(100);
const DISPATCH_TICK: Duration = Duration::from_millis(25);
const MAX_RETRY_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_LOOKUP_DURATION: Duration = Duration::from_secs(300);

fn retry_deadline(now: Instant, delay: Duration) -> Instant {
    now.checked_add(delay.min(MAX_RETRY_LIFETIME))
        .unwrap_or(now)
}

/// Callback returning whether the application accepted a downloaded torrent.
pub type TorrentAckCallback = Box<dyn Fn(TorrentInfo) -> bool + Send + Sync + 'static>;
/// Callback invoked once when an admitted Metadata job reaches a terminal state.
pub type MetadataCompletionCallback = Box<dyn Fn(MetadataFetchCompletion) + Send + Sync + 'static>;
/// Asynchronous InfoHash admission callback.
pub type MetadataFetchCallback = Box<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = MetadataAdmission> + Send>>
        + Send
        + Sync
        + 'static,
>;

/// Application admission distinguishes terminal deduplication from a temporary lease/cooldown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataAdmission {
    /// Download under the application's current claim.
    Fetch,
    /// The hash is terminally handled and needs no retry.
    Skip,
    /// Preserve candidates and check admission again after this delay.
    RetryAfter(Duration),
}

#[derive(Debug, Clone, Copy)]
/// Pending-queue capacity and maximum concurrent Metadata jobs.
pub struct MetadataSchedulerLimits {
    /// Maximum deduplicated pending InfoHashes.
    pub queue_size: usize,
    /// Maximum concurrently spawned jobs.
    pub concurrency: usize,
}

#[derive(Clone)]
/// Atomically replaceable callbacks shared with [`MetadataScheduler`].
pub struct MetadataSchedulerCallbacks {
    /// Torrent delivery callback.
    pub torrent: Arc<ArcSwapOption<TorrentAckCallback>>,
    /// Pre-download admission callback.
    pub fetch_gate: Arc<ArcSwapOption<MetadataFetchCallback>>,
    /// Terminal completion callback.
    pub completion: Arc<ArcSwapOption<MetadataCompletionCallback>>,
}

pub(crate) struct MetadataSchedulerRuntime {
    pub(crate) stats: DhtRuntimeStats,
    pub(crate) peer_lookup_tx: Option<mpsc::Sender<PeerLookupRequest>>,
    pub(crate) metadata_options: MetadataOptions,
    pub(crate) lookup_timeout: Duration,
}

#[derive(Debug, Clone, Copy)]
struct PeerCandidate {
    addr: SocketAddr,
    discovered_at: Instant,
}

struct JobPeerLookup {
    request_tx: mpsc::Sender<PeerLookupRequest>,
    info_hash: [u8; 20],
    source_nodes: Vec<NodeTuple>,
    timeout: Duration,
}

#[derive(Debug, Clone)]
struct QueuedHash {
    info_hash: String,
    peers: Vec<PeerCandidate>,
    queued_at: Instant,
    ready_at: Instant,
    latest_at: Instant,
    order_key: (Instant, u64),
    source_nodes: Vec<NodeTuple>,
    source: HashSource,
    expires_at: Instant,
    is_retry: bool,
    ready: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueuePushKind {
    Inserted,
    Updated,
    EvictedOldest,
    EvictedRetry,
    Stale,
}

#[derive(Debug)]
struct PendingHashQueue {
    capacity: usize,
    ttl: Duration,
    entries: HashMap<String, QueuedHash>,
    // Each hash has exactly one expiry and scheduling entry. Refresh never appends tombstones.
    expiry: BTreeMap<(Instant, u64), String>,
    waiting: BTreeMap<(Instant, u64), String>,
    ready_fresh: BTreeMap<(Instant, u64), String>,
    ready_retry: BTreeMap<(Instant, u64), String>,
    retry_expiry: BTreeMap<(Instant, u64), String>,
    sequence: u64,
    fresh_dispatches: u8,
}

fn merge_candidate(entry: &mut QueuedHash, event: &HashDiscovered) -> bool {
    let mut new_peer = false;
    if let Some(addr) = event.peer_addr {
        new_peer = !entry.peers.iter().any(|peer| peer.addr == addr);
        let discovered_at = entry
            .peers
            .iter()
            .find(|peer| peer.addr == addr)
            .map_or(event.discovered_at, |peer| {
                peer.discovered_at.max(event.discovered_at)
            });
        entry.peers.retain(|peer| peer.addr != addr);
        entry.peers.push(PeerCandidate {
            addr,
            discovered_at,
        });
        entry
            .peers
            .sort_unstable_by(|a, b| b.discovered_at.cmp(&a.discovered_at));
        entry.peers.truncate(MAX_METADATA_CANDIDATES_PER_HASH);
    }
    if let Some(node) = &event.source_node {
        entry.source_nodes.retain(|old| old.addr != node.addr);
        entry.source_nodes.insert(0, *node);
        entry.source_nodes.truncate(8);
    }
    if matches!(event.source, HashSource::Announce) {
        entry.source = HashSource::Announce;
    }
    entry.latest_at = entry.latest_at.max(event.discovered_at);
    new_peer
}

impl PendingHashQueue {
    fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            capacity: capacity.max(1),
            ttl,
            entries: HashMap::new(),
            expiry: BTreeMap::new(),
            waiting: BTreeMap::new(),
            ready_fresh: BTreeMap::new(),
            ready_retry: BTreeMap::new(),
            retry_expiry: BTreeMap::new(),
            sequence: 0,
            fresh_dispatches: 0,
        }
    }
    fn len(&self) -> usize {
        self.entries.len()
    }
    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    fn pressure_len(&self) -> usize {
        self.entries.len().saturating_sub(self.retry_expiry.len()) + self.ready_retry.len()
    }
    #[cfg(test)]
    fn contains(&self, hash: &str) -> bool {
        self.entries.contains_key(hash)
    }

    fn insert(&mut self, mut entry: QueuedHash) {
        self.sequence = self.sequence.wrapping_add(1);
        entry.order_key = (entry.latest_at, self.sequence);
        self.expiry
            .insert((entry.expires_at, self.sequence), entry.info_hash.clone());
        if entry.is_retry {
            self.retry_expiry
                .insert((entry.expires_at, self.sequence), entry.info_hash.clone());
        }
        if entry.ready {
            if entry.is_retry {
                self.ready_retry
                    .insert((entry.ready_at, self.sequence), entry.info_hash.clone());
            } else {
                self.ready_fresh
                    .insert(entry.order_key, entry.info_hash.clone());
            }
        } else {
            self.waiting
                .insert((entry.ready_at, self.sequence), entry.info_hash.clone());
        }
        self.entries.insert(entry.info_hash.clone(), entry);
    }

    fn push(&mut self, event: HashDiscovered, now: Instant) -> QueuePushKind {
        if now
            .checked_duration_since(event.discovered_at)
            .unwrap_or_default()
            > self.ttl
        {
            return QueuePushKind::Stale;
        }
        if let Some(mut entry) = self.remove(&event.info_hash) {
            merge_candidate(&mut entry, &event);
            if !entry.is_retry {
                entry.expires_at = entry.latest_at + self.ttl;
            }
            self.insert(entry);
            return QueuePushKind::Updated;
        }
        let mut result = QueuePushKind::Inserted;
        if self.entries.len() >= self.capacity {
            let Some((_, oldest_hash)) = self
                .retry_expiry
                .first_key_value()
                .or_else(|| self.expiry.first_key_value())
            else {
                return QueuePushKind::Stale;
            };
            let oldest_hash = oldest_hash.clone();
            let oldest = &self.entries[&oldest_hash];
            if !oldest.is_retry && event.discovered_at <= oldest.latest_at {
                return QueuePushKind::Stale;
            }
            result = if oldest.is_retry {
                QueuePushKind::EvictedRetry
            } else {
                QueuePushKind::EvictedOldest
            };
            self.remove(&oldest_hash);
        }
        let mut entry = QueuedHash {
            info_hash: event.info_hash.clone(),
            peers: Vec::new(),
            source_nodes: Vec::new(),
            source: event.source,
            queued_at: now,
            ready_at: now + PEER_COALESCE_WINDOW,
            latest_at: event.discovered_at,
            order_key: (now, 0),
            expires_at: event.discovered_at + self.ttl,
            is_retry: false,
            ready: false,
        };
        merge_candidate(&mut entry, &event);
        self.insert(entry);
        result
    }

    fn remove(&mut self, hash: &str) -> Option<QueuedHash> {
        let entry = self.entries.remove(hash)?;
        let sequence = entry.order_key.1;
        self.expiry.remove(&(entry.expires_at, sequence));
        self.retry_expiry.remove(&(entry.expires_at, sequence));
        self.waiting.remove(&(entry.ready_at, sequence));
        self.ready_fresh.remove(&entry.order_key);
        self.ready_retry.remove(&(entry.ready_at, sequence));
        Some(entry)
    }

    fn defer(&mut self, mut entry: QueuedHash, record: RetryRecord) -> bool {
        self.remove(&entry.info_hash);
        if self.entries.len() >= self.capacity
            || self.retry_expiry.len() >= (self.capacity / 4).max(1)
        {
            return false;
        }
        entry.ready_at = record.next_due;
        entry.expires_at = record.expires_at;
        entry.is_retry = true;
        entry.ready = false;
        self.insert(entry);
        true
    }

    fn promote_ready(&mut self, now: Instant) {
        while let Some((&key, _)) = self.waiting.first_key_value() {
            if key.0 > now {
                break;
            }
            let (_, hash) = self.waiting.pop_first().unwrap();
            let entry = self.entries.get_mut(&hash).expect("indexed queued hash");
            entry.ready = true;
            if entry.is_retry {
                self.ready_retry.insert(key, hash);
            } else {
                self.ready_fresh.insert(entry.order_key, hash);
            }
        }
    }

    fn pop_newest_ready(
        &mut self,
        now: Instant,
        _in_flight: &HashMap<String, InFlightState>,
    ) -> Option<QueuedHash> {
        self.expire(now);
        self.promote_ready(now);
        // At most three fresh jobs before one due retry. Either class borrows idle capacity.
        let use_retry = !self.ready_retry.is_empty()
            && (self.fresh_dispatches >= 3 || self.ready_fresh.is_empty());
        let hash = if use_retry {
            self.fresh_dispatches = 0;
            self.ready_retry.first_key_value()?.1.clone()
        } else {
            self.fresh_dispatches = self.fresh_dispatches.saturating_add(1);
            self.ready_fresh.last_key_value()?.1.clone()
        };
        self.remove(&hash)
    }

    fn expire(&mut self, now: Instant) -> usize {
        let mut expired = 0;
        while let Some((&key, hash)) = self.expiry.first_key_value() {
            if key.0 >= now {
                break;
            }
            let hash = hash.clone();
            self.remove(&hash);
            expired += 1;
        }
        expired
    }
}

#[derive(Debug, Clone, Copy)]
struct RetryRecord {
    failures: usize,
    next_due: Instant,
    expires_at: Instant,
    extra_attempts: usize,
}

impl RetryRecord {
    fn attempt_limit(self, policy: &MetadataOptions) -> usize {
        policy
            .retry_max_attempts
            .saturating_add(self.extra_attempts)
    }
}

struct RetryState {
    record: RetryRecord,
    peers: Vec<SocketAddr>,
    sources: Vec<SocketAddr>,
}

impl RetryState {
    fn observe(&mut self, peer: Option<SocketAddr>, source: Option<SocketAddr>) -> bool {
        let mut novel = false;
        for (candidate, seen, capacity) in [
            (peer, &mut self.peers, MAX_METADATA_CANDIDATES_PER_HASH),
            (source, &mut self.sources, 8),
        ] {
            if let Some(addr) = candidate
                && !seen.contains(&addr)
            {
                novel = true;
                if seen.len() < capacity {
                    seen.push(addr);
                }
            }
        }
        novel
    }
}

#[derive(Default)]
struct RetryHistory {
    entries: HashMap<String, RetryState>,
    expiry: BTreeMap<(Instant, String), ()>,
}

impl RetryHistory {
    fn expire(&mut self, now: Instant) {
        while let Some(((at, _), _)) = self.expiry.first_key_value() {
            if *at > now {
                break;
            }
            let ((_, hash), _) = self.expiry.pop_first().unwrap();
            self.entries.remove(&hash);
        }
    }
    fn remove(&mut self, hash: &str) {
        if let Some(state) = self.entries.remove(hash) {
            self.expiry
                .remove(&(state.record.expires_at, hash.to_string()));
        }
    }
    fn record(
        &mut self,
        hash: &str,
        now: Instant,
        failed: bool,
        delay: Duration,
        policy: &MetadataOptions,
    ) -> Option<RetryRecord> {
        self.expire(now);
        if !self.entries.contains_key(hash) {
            if self.entries.len() >= policy.retry_history_capacity {
                return None;
            }
            let expires_at =
                retry_deadline(now, Duration::from_secs(policy.retry_history_ttl_secs));
            self.entries.insert(
                hash.to_string(),
                RetryState {
                    record: RetryRecord {
                        failures: 0,
                        next_due: now,
                        expires_at,
                        extra_attempts: 0,
                    },
                    peers: Vec::new(),
                    sources: Vec::new(),
                },
            );
            self.expiry.insert((expires_at, hash.to_string()), ());
        }
        let record = &mut self.entries.get_mut(hash).unwrap().record;
        if failed {
            record.failures = record.failures.saturating_add(1);
        }
        let backoff = if failed {
            let multiplier = 1u64 << record.failures.saturating_sub(1).min(31);
            Duration::from_secs(
                policy
                    .retry_initial_delay_secs
                    .saturating_mul(multiplier)
                    .min(policy.retry_max_delay_secs),
            )
        } else {
            delay
        };
        // A zero-duration application deferral must not become a busy admission loop.
        record.next_due = retry_deadline(now, backoff.max(Duration::from_millis(100)));
        Some(*record)
    }

    fn observe_event(
        &mut self,
        event: &HashDiscovered,
        now: Instant,
        policy: &MetadataOptions,
    ) -> Option<RetryRecord> {
        self.expire(now);
        let state = self.entries.get_mut(&event.info_hash)?;
        let novel = state.observe(
            event.peer_addr,
            event.source_node.as_ref().map(|node| node.addr),
        );
        if state.record.failures >= state.record.attempt_limit(policy)
            && novel
            && state.record.extra_attempts < 2
            && policy.retry_max_attempts > 0
        {
            state.record.extra_attempts += 1;
            state.record.next_due = retry_deadline(
                now,
                Duration::from_secs(policy.retry_initial_delay_secs)
                    .max(Duration::from_millis(100)),
            );
        }
        Some(state.record)
    }

    fn remember(&mut self, entry: &QueuedHash) {
        if let Some(state) = self.entries.get_mut(&entry.info_hash) {
            for peer in &entry.peers {
                state.observe(Some(peer.addr), None);
            }
            for node in &entry.source_nodes {
                state.observe(None, Some(node.addr));
            }
        }
    }
}

#[derive(Debug)]
struct MetadataJob {
    info_hash: String,
    peers: Vec<PeerCandidate>,
    peer_rx: mpsc::Receiver<PeerCandidate>,
    source_nodes: Vec<NodeTuple>,
    source: HashSource,
}

#[derive(Debug)]
struct InFlightState {
    peer_tx: mpsc::Sender<PeerCandidate>,
    scheduled_peers: HashSet<SocketAddr>,
    entry: QueuedHash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobOutcome {
    GateRejected,
    Deferred(Duration),
    Completed(MetadataFetchCompletionStatus),
}

#[derive(Debug)]
struct JobResult {
    info_hash: String,
    attempts: usize,
    outcome: JobOutcome,
}

/// Bounded, deduplicating, freshness-aware Metadata job scheduler.
pub struct MetadataScheduler {
    hash_rx: mpsc::Receiver<HashDiscovered>,
    max_queue_size: usize,
    max_concurrent: usize,
    fetcher: Arc<RbitFetcher>,
    callback: Arc<ArcSwapOption<TorrentAckCallback>>,
    on_metadata_fetch: Arc<ArcSwapOption<MetadataFetchCallback>>,
    completion_callback: Arc<ArcSwapOption<MetadataCompletionCallback>>,
    total_received: Arc<AtomicU64>,
    total_dropped: Arc<AtomicU64>,
    total_dispatched: Arc<AtomicU64>,
    total_completed: Arc<AtomicU64>,
    queue_len: Arc<AtomicUsize>,
    runtime_stats: DhtRuntimeStats,
    peer_lookup_tx: Option<mpsc::Sender<PeerLookupRequest>>,
    metadata_options: MetadataOptions,
    lookup_timeout: Duration,
    retry_history: Mutex<RetryHistory>,
    shutdown: CancellationToken,
}

impl MetadataScheduler {
    /// Creates a scheduler using local runtime statistics.
    pub fn new(
        hash_rx: mpsc::Receiver<HashDiscovered>,
        fetcher: Arc<RbitFetcher>,
        limits: MetadataSchedulerLimits,
        callbacks: MetadataSchedulerCallbacks,
        queue_len: Arc<AtomicUsize>,
        shutdown: CancellationToken,
    ) -> Self {
        Self::new_with_runtime_stats(
            hash_rx,
            fetcher,
            limits,
            callbacks,
            queue_len,
            shutdown,
            DhtRuntimeStats::default(),
        )
    }

    pub(crate) fn new_with_runtime_stats(
        hash_rx: mpsc::Receiver<HashDiscovered>,
        fetcher: Arc<RbitFetcher>,
        limits: MetadataSchedulerLimits,
        callbacks: MetadataSchedulerCallbacks,
        queue_len: Arc<AtomicUsize>,
        shutdown: CancellationToken,
        runtime_stats: DhtRuntimeStats,
    ) -> Self {
        Self::new_with_runtime(
            hash_rx,
            fetcher,
            limits,
            callbacks,
            queue_len,
            shutdown,
            MetadataSchedulerRuntime {
                stats: runtime_stats,
                peer_lookup_tx: None,
                metadata_options: MetadataOptions::default(),
                lookup_timeout: Duration::from_millis(2_000),
            },
        )
    }

    pub(crate) fn new_with_runtime(
        hash_rx: mpsc::Receiver<HashDiscovered>,
        fetcher: Arc<RbitFetcher>,
        limits: MetadataSchedulerLimits,
        callbacks: MetadataSchedulerCallbacks,
        queue_len: Arc<AtomicUsize>,
        shutdown: CancellationToken,
        runtime: MetadataSchedulerRuntime,
    ) -> Self {
        Self {
            hash_rx,
            max_queue_size: limits.queue_size.max(1),
            max_concurrent: limits.concurrency.max(1),
            fetcher,
            callback: callbacks.torrent,
            on_metadata_fetch: callbacks.fetch_gate,
            completion_callback: callbacks.completion,
            total_received: Arc::new(AtomicU64::new(0)),
            total_dropped: Arc::new(AtomicU64::new(0)),
            total_dispatched: Arc::new(AtomicU64::new(0)),
            total_completed: Arc::new(AtomicU64::new(0)),
            queue_len,
            runtime_stats: runtime.stats,
            peer_lookup_tx: runtime.peer_lookup_tx,
            metadata_options: runtime.metadata_options,
            lookup_timeout: runtime.lookup_timeout.min(MAX_LOOKUP_DURATION),
            retry_history: Mutex::new(RetryHistory::default()),
            shutdown,
        }
    }

    /// Replaces the torrent delivery callback.
    pub fn set_callback(&mut self, callback: Arc<TorrentAckCallback>) {
        self.callback.store(Some(callback));
    }

    /// Replaces the pre-download admission callback.
    pub fn set_metadata_fetch_callback(&mut self, callback: Arc<MetadataFetchCallback>) {
        self.on_metadata_fetch.store(Some(callback));
    }

    /// Replaces the terminal completion callback.
    pub fn set_completion_callback(&mut self, callback: Arc<MetadataCompletionCallback>) {
        self.completion_callback.store(Some(callback));
    }

    /// Runs until cancellation and drains completed jobs before returning when input closes.
    pub async fn run(mut self) {
        let mut queue = PendingHashQueue::new(self.max_queue_size, HASH_QUEUE_TTL);
        let mut in_flight = HashMap::<String, InFlightState>::new();
        let mut tasks = JoinSet::<JobResult>::new();
        let mut task_hashes = HashMap::new();
        let mut maintenance = tokio::time::interval(Duration::from_secs(1));
        maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut dispatch_tick = tokio::time::interval(DISPATCH_TICK);
        dispatch_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut stats_interval = tokio::time::interval(Duration::from_secs(60));
        stats_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut input_closed = false;

        loop {
            self.dispatch_ready(&mut queue, &mut in_flight, &mut tasks, &mut task_hashes);
            self.sync_queue_len(queue.len(), in_flight.len());
            // Cooling retries consume bounded storage, but are not runnable metadata pressure.
            self.queue_len.store(
                queue.pressure_len().saturating_add(in_flight.len()),
                Ordering::Relaxed,
            );
            self.runtime_stats.yield_stats().set(
                "metadata_retry_waiting",
                queue
                    .retry_expiry
                    .len()
                    .saturating_sub(queue.ready_retry.len()) as u64,
            );
            let history_len = self
                .retry_history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .entries
                .len();
            self.runtime_stats
                .yield_stats()
                .set("metadata_retry_history", history_len as u64);

            if input_closed && queue.is_empty() && tasks.is_empty() {
                break;
            }

            tokio::select! {
                _ = self.shutdown.cancelled() => break,
                result = self.hash_rx.recv(), if !input_closed => {
                    self.runtime_stats
                        .set_hash_ingress_queue_depth(self.hash_rx.len());
                    match result {
                        Some(hash) => self.enqueue(&mut queue, &mut in_flight, hash),
                        None => input_closed = true,
                    }
                }
                result = tasks.join_next_with_id(), if !tasks.is_empty() => {
                    match result {
                        Some(Ok((id, result))) => {
                            task_hashes.remove(&id);
                            self.handle_job_result(result, &mut queue, &mut in_flight);
                        }
                        Some(Err(error)) => {
                            #[cfg(feature = "metrics")]
                            counter!("dht_metadata_worker_join_error_total").increment(1);
                            log::warn!("Metadata worker task failed: {error}");
                            if let Some(info_hash) = task_hashes.remove(&error.id()) {
                                // The application fences cleanup by its actual claim, including
                                // a claim acquired before admission panicked.
                                self.handle_job_result(JobResult {
                                    info_hash, attempts: 0,
                                    outcome: JobOutcome::Completed(MetadataFetchCompletionStatus::FetchFailed),
                                }, &mut queue, &mut in_flight);
                            }
                        }
                        None => {}
                    }
                }
                _ = dispatch_tick.tick() => {}
                _ = maintenance.tick() => {
                    let expired = queue.expire(Instant::now());
                    if expired > 0 {
                        self.total_dropped.fetch_add(expired as u64, Ordering::Relaxed);
                        self.runtime_stats.metadata_queue_stale(expired);
                        #[cfg(feature = "metrics")]
                        counter!("dht_metadata_queue_events_total", "result" => "expired")
                            .increment(expired as u64);
                    }
                }
                _ = stats_interval.tick() => self.print_stats_inline(),
            }
        }

        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
        self.queue_len.store(0, Ordering::Relaxed);
        self.runtime_stats.set_hash_ingress_queue_depth(0);
        self.runtime_stats.set_metadata_queue(0, 0);
        self.runtime_stats
            .yield_stats()
            .set("metadata_retry_waiting", 0);
        self.runtime_stats
            .yield_stats()
            .set("metadata_retry_history", 0);
        #[cfg(feature = "metrics")]
        gauge!("dht_metadata_queue_depth").set(0.0);
    }

    fn enqueue(
        &self,
        queue: &mut PendingHashQueue,
        in_flight: &mut HashMap<String, InFlightState>,
        hash: HashDiscovered,
    ) {
        self.total_received.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        if let Some(state) = in_flight.get_mut(&hash.info_hash) {
            if now
                .checked_duration_since(hash.discovered_at)
                .unwrap_or_default()
                > HASH_QUEUE_TTL
            {
                self.record_queue_push(QueuePushKind::Stale);
                return;
            }
            merge_candidate(&mut state.entry, &hash);
            if let Some(addr) = hash.peer_addr
                && !state.scheduled_peers.contains(&addr)
                && state.scheduled_peers.len() < MAX_METADATA_CANDIDATES_PER_HASH
                && state
                    .peer_tx
                    .try_send(PeerCandidate {
                        addr,
                        discovered_at: hash.discovered_at,
                    })
                    .is_ok()
            {
                state.scheduled_peers.insert(addr);
            }
            self.record_queue_push(QueuePushKind::Updated);
            return;
        }
        let record = {
            let mut history = self
                .retry_history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            history.observe_event(&hash, now, &self.metadata_options)
        };
        if record
            .is_some_and(|record| record.failures >= record.attempt_limit(&self.metadata_options))
        {
            self.runtime_stats
                .yield_stats()
                .inc("metadata_retry_exhausted");
            return;
        }
        if let Some(entry) = queue.entries.get(&hash.info_hash)
            && entry.is_retry
            && hash
                .peer_addr
                .is_some_and(|addr| !entry.peers.iter().any(|peer| peer.addr == addr))
        {
            self.runtime_stats
                .yield_stats()
                .inc("metadata_retry_new_peer");
        }
        let info_hash = hash.info_hash.clone();
        let kind = queue.push(hash, now);
        if let Some(record) = record
            && let Some(entry) = queue.remove(&info_hash)
        {
            queue.defer(entry, record);
        }
        self.record_queue_push(kind);
    }

    fn record_queue_push(&self, kind: QueuePushKind) {
        let result = match kind {
            QueuePushKind::Inserted => {
                self.runtime_stats.metadata_queue_inserted();
                "inserted"
            }
            QueuePushKind::Updated => {
                self.runtime_stats.metadata_queue_deduplicated();
                "deduplicated"
            }
            QueuePushKind::EvictedOldest | QueuePushKind::EvictedRetry => {
                if kind == QueuePushKind::EvictedRetry {
                    self.runtime_stats
                        .yield_stats()
                        .inc("metadata_retry_evicted");
                }
                self.total_dropped.fetch_add(1, Ordering::Relaxed);
                self.runtime_stats.metadata_queue_inserted();
                self.runtime_stats.metadata_queue_evicted();
                "evicted_oldest"
            }
            QueuePushKind::Stale => {
                self.total_dropped.fetch_add(1, Ordering::Relaxed);
                self.runtime_stats.metadata_queue_stale(1);
                "stale"
            }
        };
        #[cfg(feature = "metrics")]
        counter!("dht_metadata_queue_events_total", "result" => result).increment(1);
        #[cfg(not(feature = "metrics"))]
        let _ = result;
    }

    fn dispatch_ready(
        &self,
        queue: &mut PendingHashQueue,
        in_flight: &mut HashMap<String, InFlightState>,
        tasks: &mut JoinSet<JobResult>,
        task_hashes: &mut HashMap<tokio::task::Id, String>,
    ) {
        queue.promote_ready(Instant::now());
        while tasks.len() < self.max_concurrent {
            let Some(mut entry) = queue.pop_newest_ready(Instant::now(), in_flight) else {
                break;
            };
            let queue_wait = Instant::now()
                .checked_duration_since(entry.queued_at)
                .unwrap_or_default();
            self.runtime_stats.observe_metadata_queue_wait(
                queue_wait.as_millis().min(u128::from(u64::MAX)) as u64,
            );
            #[cfg(feature = "metrics")]
            histogram!("dht_metadata_queue_wait_seconds").record(queue_wait.as_secs_f64());
            let now = Instant::now();
            entry.peers.retain(|peer| {
                if entry.is_retry {
                    return true;
                }
                now.checked_duration_since(peer.discovered_at)
                    .unwrap_or_default()
                    <= HASH_QUEUE_TTL
            });
            entry.peers.truncate(MAX_METADATA_CANDIDATES_PER_HASH);
            if entry.peers.is_empty() && self.peer_lookup_tx.is_none() {
                self.total_dropped.fetch_add(1, Ordering::Relaxed);
                self.runtime_stats.metadata_queue_stale(1);
                continue;
            }
            let (peer_tx, peer_rx) = mpsc::channel(MAX_METADATA_CANDIDATES_PER_HASH);
            let state = InFlightState {
                peer_tx,
                scheduled_peers: entry.peers.iter().map(|peer| peer.addr).collect(),
                entry: entry.clone(),
            };
            if entry.is_retry {
                self.runtime_stats
                    .yield_stats()
                    .inc("metadata_retry_dispatched");
            }
            in_flight.insert(entry.info_hash.clone(), state);
            self.spawn_job(entry, peer_rx, tasks, task_hashes);
        }
    }

    fn spawn_job(
        &self,
        entry: QueuedHash,
        peer_rx: mpsc::Receiver<PeerCandidate>,
        tasks: &mut JoinSet<JobResult>,
        task_hashes: &mut HashMap<tokio::task::Id, String>,
    ) {
        self.total_dispatched.fetch_add(1, Ordering::Relaxed);
        #[cfg(feature = "metrics")]
        counter!("dht_metadata_jobs_dispatched_total").increment(1);

        let job = MetadataJob {
            info_hash: entry.info_hash,
            peers: entry.peers,
            peer_rx,
            source_nodes: entry.source_nodes,
            source: entry.source,
        };
        let fetcher = self.fetcher.clone();
        let callback = self.callback.clone();
        let on_metadata_fetch = self.on_metadata_fetch.clone();
        let runtime_stats = self.runtime_stats.clone();
        let peer_lookup_tx = self.peer_lookup_tx.clone();
        let lookup_timeout = self.lookup_timeout;
        let info_hash = job.info_hash.clone();
        let handle = tasks.spawn(async move {
            Self::process_hash(
                job,
                &fetcher,
                &callback,
                &on_metadata_fetch,
                &runtime_stats,
                peer_lookup_tx,
                lookup_timeout,
            )
            .await
        });
        task_hashes.insert(handle.id(), info_hash);
    }

    fn handle_job_result(
        &self,
        result: JobResult,
        queue: &mut PendingHashQueue,
        in_flight: &mut HashMap<String, InFlightState>,
    ) {
        let Some(state) = in_flight.remove(&result.info_hash) else {
            return;
        };
        queue.remove(&result.info_hash);
        match result.outcome {
            JobOutcome::GateRejected => {
                self.retry_history
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&result.info_hash);
                #[cfg(feature = "metrics")]
                counter!("dht_metadata_jobs_completed_total", "result" => "gate_rejected")
                    .increment(1);
            }
            JobOutcome::Deferred(delay) => {
                self.runtime_stats
                    .yield_stats()
                    .inc("metadata_retry_deferred");
                self.schedule_retry(state.entry, queue, false, delay);
            }
            JobOutcome::Completed(status) => {
                self.finish(result.info_hash.clone(), status, result.attempts);
                if status == MetadataFetchCompletionStatus::Accepted {
                    self.retry_history
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&result.info_hash);
                } else {
                    self.schedule_retry(state.entry, queue, true, Duration::ZERO);
                }
            }
        }
    }

    fn schedule_retry(
        &self,
        entry: QueuedHash,
        queue: &mut PendingHashQueue,
        failed: bool,
        delay: Duration,
    ) {
        if self.metadata_options.retry_max_attempts == 0 {
            return;
        }
        let now = Instant::now();
        if entry.is_retry && entry.expires_at <= now {
            self.runtime_stats
                .yield_stats()
                .inc("metadata_retry_exhausted");
            return;
        }
        let record = {
            let mut history = self
                .retry_history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let record =
                history.record(&entry.info_hash, now, failed, delay, &self.metadata_options);
            history.remember(&entry);
            record
        };
        let Some(record) = record else {
            self.runtime_stats
                .yield_stats()
                .inc("metadata_retry_history_full");
            return;
        };
        if record.failures >= record.attempt_limit(&self.metadata_options)
            || record.next_due >= record.expires_at
        {
            self.runtime_stats
                .yield_stats()
                .inc("metadata_retry_exhausted");
            return;
        }
        if queue.defer(entry, record) {
            self.runtime_stats
                .yield_stats()
                .inc("metadata_retry_scheduled");
        } else {
            self.runtime_stats
                .yield_stats()
                .inc("metadata_retry_evicted");
        }
    }

    fn finish(&self, info_hash: String, status: MetadataFetchCompletionStatus, attempts: usize) {
        self.total_completed.fetch_add(1, Ordering::Relaxed);
        let result = match status {
            MetadataFetchCompletionStatus::Accepted => "accepted",
            MetadataFetchCompletionStatus::FetchFailed => "fetch_failed",
            MetadataFetchCompletionStatus::DeliveryRejected => "delivery_rejected",
        };
        #[cfg(feature = "metrics")]
        counter!("dht_metadata_jobs_completed_total", "result" => result).increment(1);
        #[cfg(not(feature = "metrics"))]
        let _ = result;

        if let Some(callback) = self.completion_callback.load_full() {
            let completion = MetadataFetchCompletion {
                info_hash,
                status,
                attempts,
            };
            if catch_unwind(AssertUnwindSafe(|| callback(completion))).is_err() {
                #[cfg(feature = "metrics")]
                counter!("dht_metadata_completion_callback_panics_total").increment(1);
                log::warn!("Metadata completion callback panicked");
            }
        }
    }

    async fn process_hash(
        mut job: MetadataJob,
        fetcher: &Arc<RbitFetcher>,
        callback: &Arc<ArcSwapOption<TorrentAckCallback>>,
        on_metadata_fetch: &Arc<ArcSwapOption<MetadataFetchCallback>>,
        runtime_stats: &DhtRuntimeStats,
        peer_lookup_tx: Option<mpsc::Sender<PeerLookupRequest>>,
        lookup_timeout: Duration,
    ) -> JobResult {
        if let Some(gate) = on_metadata_fetch.load_full() {
            let outcome = match gate(job.info_hash.clone()).await {
                MetadataAdmission::Fetch => None,
                MetadataAdmission::Skip => Some(JobOutcome::GateRejected),
                MetadataAdmission::RetryAfter(delay) => Some(JobOutcome::Deferred(delay)),
            };
            if let Some(outcome) = outcome {
                return JobResult {
                    info_hash: job.info_hash,
                    attempts: 0,
                    outcome,
                };
            }
        }
        runtime_stats.yield_stats().inc(match job.source {
            HashSource::Announce => "metadata_source_announce_started",
            HashSource::GetPeers => "metadata_source_get_peers_started",
            HashSource::Sample => "metadata_source_sample_started",
        });
        if job.peers.is_empty() {
            runtime_stats
                .yield_stats()
                .inc("metadata_hash_only_started");
        }

        let info_hash_bytes: [u8; 20] = match hex::decode(&job.info_hash) {
            Ok(bytes) if bytes.len() == 20 => {
                let mut hash = [0u8; 20];
                hash.copy_from_slice(&bytes);
                hash
            }
            _ => {
                return JobResult {
                    info_hash: job.info_hash,
                    attempts: 0,
                    outcome: JobOutcome::Completed(MetadataFetchCompletionStatus::FetchFailed),
                };
            }
        };

        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_fetch = attempts.clone();
        let fetcher_for_race = fetcher.clone();
        let lookup = peer_lookup_tx.map(|request_tx| JobPeerLookup {
            request_tx,
            info_hash: info_hash_bytes,
            source_nodes: job.source_nodes,
            timeout: lookup_timeout,
        });
        let fetched = race_peer_fetches(
            job.peers,
            &mut job.peer_rx,
            lookup,
            move |peer| {
                let attempts = attempts_for_fetch.clone();
                let fetcher = fetcher_for_race.clone();
                async move {
                    let outcome = fetcher
                        .fetch_with_attempt_observer(&info_hash_bytes, peer.addr, move || {
                            attempts.fetch_add(1, Ordering::Relaxed);
                        })
                        .await;
                    (peer, outcome)
                }
            },
            runtime_stats,
        )
        .await;
        let attempts = attempts.load(Ordering::Relaxed);

        if let Some((peer, (name, total_size, files, piece_length))) = fetched {
            runtime_stats.yield_stats().inc(match job.source {
                HashSource::Announce => "metadata_source_announce_succeeded",
                HashSource::GetPeers => "metadata_source_get_peers_succeeded",
                HashSource::Sample => "metadata_source_sample_succeeded",
            });
            let metadata = TorrentInfo {
                info_hash: job.info_hash.clone(),
                name,
                total_size,
                files,
                magnet_link: format!("magnet:?xt=urn:btih:{}", job.info_hash),
                peers: vec![peer.addr.to_string()],
                piece_length,
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            };

            let accepted = callback.load_full().is_some_and(|callback| {
                catch_unwind(AssertUnwindSafe(|| callback(metadata))).unwrap_or(false)
            });
            let status = if accepted {
                MetadataFetchCompletionStatus::Accepted
            } else {
                MetadataFetchCompletionStatus::DeliveryRejected
            };
            return JobResult {
                info_hash: job.info_hash,
                attempts,
                outcome: JobOutcome::Completed(status),
            };
        }

        runtime_stats.yield_stats().inc(match job.source {
            HashSource::Announce => "metadata_source_announce_failed",
            HashSource::GetPeers => "metadata_source_get_peers_failed",
            HashSource::Sample => "metadata_source_sample_failed",
        });
        JobResult {
            info_hash: job.info_hash,
            attempts,
            outcome: JobOutcome::Completed(MetadataFetchCompletionStatus::FetchFailed),
        }
    }

    fn sync_queue_len(&self, queued: usize, in_flight: usize) {
        let depth = queued.saturating_add(in_flight);
        self.queue_len.store(depth, Ordering::Relaxed);
        self.runtime_stats.set_metadata_queue(depth, in_flight);
        #[cfg(feature = "metrics")]
        {
            gauge!("dht_metadata_queue_depth").set(depth as f64);
            gauge!("dht_metadata_in_flight").set(in_flight as f64);
        }
    }

    fn print_stats_inline(&self) {
        #[cfg(debug_assertions)]
        {
            let received = self.total_received.load(Ordering::Relaxed);
            let dropped = self.total_dropped.load(Ordering::Relaxed);
            let dispatched = self.total_dispatched.load(Ordering::Relaxed);
            let completed = self.total_completed.load(Ordering::Relaxed);
            let queue_len = self.queue_len.load(Ordering::Relaxed);
            let queue_pressure = queue_len as f64 / self.max_queue_size as f64 * 100.0;
            log::info!(
                "Metadata scheduler: depth={}/{}, pressure={:.1}%, received={}, dispatched={}, completed={}, evicted_or_expired={}",
                queue_len,
                self.max_queue_size,
                queue_pressure,
                received,
                dispatched,
                completed,
                dropped,
            );
        }
    }
}

fn queue_race_candidate(
    peer: PeerCandidate,
    source: &'static str,
    seen: &mut HashSet<SocketAddr>,
    pending: &mut VecDeque<(PeerCandidate, &'static str)>,
) {
    if seen.len() < MAX_METADATA_CANDIDATES_PER_HASH && seen.insert(peer.addr) {
        pending.push_back((peer, source));
    }
}

async fn race_peer_fetches<F, Fut>(
    peers: Vec<PeerCandidate>,
    peer_rx: &mut mpsc::Receiver<PeerCandidate>,
    lookup: Option<JobPeerLookup>,
    fetch: F,
    runtime_stats: &DhtRuntimeStats,
) -> Option<(PeerCandidate, FetchedMetadata)>
where
    F: Fn(PeerCandidate) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = (PeerCandidate, MetadataFetchOutcome)> + Send + 'static,
{
    runtime_stats.metadata_race_started();
    #[cfg(feature = "metrics")]
    counter!("dht_metadata_peer_races_total").increment(1);

    let mut tasks = JoinSet::new();
    let mut seen_peers = HashSet::new();
    let mut pending_peers = VecDeque::new();
    for peer in peers {
        queue_race_candidate(peer, "initial", &mut seen_peers, &mut pending_peers);
    }
    // Reserve one potential attempt for every outstanding fetch. A cache skip refunds
    // its reservation, while any real attempt keeps it, even after a fast failure.
    // This bounds both simultaneous TCP connections and total actual attempts to 12.
    let mut attempts_reserved = 0usize;

    let mut lookup_open = lookup.is_some();
    let lookup_timeout = lookup.as_ref().map_or(Duration::ZERO, |lookup| {
        lookup.timeout.min(MAX_LOOKUP_DURATION)
    });
    let mut lookup_submitted = !lookup_open;
    let (lookup_peer_tx, mut lookup_peer_rx) = mpsc::channel(MAX_METADATA_CANDIDATES_PER_HASH);
    let submit_lookup = async move {
        if let Some(lookup) = lookup {
            tokio::time::sleep(ACTIVE_PEER_LOOKUP_DELAY).await;
            // Full/closed request queues drop the result sender and promptly end the wait.
            if lookup
                .request_tx
                .try_send(PeerLookupRequest {
                    info_hash: lookup.info_hash,
                    peer_tx: lookup_peer_tx,
                    source_nodes: lookup.source_nodes,
                })
                .is_err()
            {
                runtime_stats.yield_stats().inc("lookup_request_queue_full");
            }
        }
    };
    tokio::pin!(submit_lookup);
    // Includes request-channel delay and a small scheduling allowance. The actor has its
    // own deadline too, but a stalled or cancelled actor must never hold a job indefinitely.
    let lookup_deadline = tokio::time::Instant::now()
        + ACTIVE_PEER_LOOKUP_DELAY
        + lookup_timeout
        + LIVE_PEER_IDLE_GRACE;
    let mut lookup_deadline_elapsed = false;
    let mut accepting_live_peers = true;
    loop {
        while attempts_reserved < MAX_METADATA_PEERS_PER_HASH {
            let Some((peer, source)) = pending_peers.pop_front() else {
                break;
            };
            attempts_reserved += 1;
            tasks.spawn(fetch.clone()(peer));
            runtime_stats.metadata_peer_candidate();
            if source != "initial" {
                runtime_stats.metadata_live_peer_join();
                #[cfg(feature = "metrics")]
                counter!("dht_metadata_live_peer_joins_total").increment(1);
            }
            #[cfg(feature = "metrics")]
            counter!("dht_metadata_peer_candidates_total", "source" => source).increment(1);
        }
        if seen_peers.len() >= MAX_METADATA_CANDIDATES_PER_HASH {
            accepting_live_peers = false;
            lookup_open = false;
            peer_rx.close();
            lookup_peer_rx.close();
        }
        if tasks.is_empty()
            && (attempts_reserved >= MAX_METADATA_PEERS_PER_HASH
                || (pending_peers.is_empty() && !accepting_live_peers && !lookup_open))
        {
            return None;
        }

        tokio::select! {
            result = tasks.join_next(), if !tasks.is_empty() => {
                let Some(Ok((peer, outcome))) = result else { continue; };
                match outcome {
                    MetadataFetchOutcome::SkippedCached => {
                        attempts_reserved = attempts_reserved.saturating_sub(1);
                    }
                    MetadataFetchOutcome::Failed => {},
                    MetadataFetchOutcome::Fetched(metadata) => {
                        let canceled = tasks.len();
                        runtime_stats.metadata_peer_canceled(canceled);
                        #[cfg(feature = "metrics")]
                        counter!("dht_metadata_peer_canceled_total").increment(canceled as u64);
                        peer_rx.close();
                        lookup_peer_rx.close();
                        tasks.abort_all();
                        while tasks.join_next().await.is_some() {}
                        return Some((peer, metadata));
                    }
                }
            }
            peer = peer_rx.recv(), if accepting_live_peers => {
                match peer {
                    Some(peer) => queue_race_candidate(peer, "live", &mut seen_peers, &mut pending_peers),
                    None => accepting_live_peers = false,
                }
            }
            peer = lookup_peer_rx.recv(), if lookup_open => {
                match peer {
                    Some(addr) => queue_race_candidate(PeerCandidate { addr, discovered_at: Instant::now() }, "lookup", &mut seen_peers, &mut pending_peers),
                    None => lookup_open = false,
                }
            }
            _ = &mut submit_lookup, if lookup_open && !lookup_submitted => {
                lookup_submitted = true;
            }
            _ = tokio::time::sleep_until(lookup_deadline),
                if lookup_open && !lookup_deadline_elapsed => {
                lookup_peer_rx.close();
                lookup_deadline_elapsed = true;
                // Drain any already-delivered peers before treating the lookup as complete.
            }
            _ = tokio::time::sleep(LIVE_PEER_IDLE_GRACE),
                if tasks.is_empty() && pending_peers.is_empty() && !lookup_open => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    struct DropSignal(Arc<AtomicBool>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    fn empty_torrent_callback() -> Arc<ArcSwapOption<TorrentAckCallback>> {
        Arc::new(ArcSwapOption::empty())
    }

    fn fetch_gate<F, Fut>(callback: F) -> Arc<ArcSwapOption<MetadataFetchCallback>>
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = bool> + Send + 'static,
    {
        let holder = Arc::new(ArcSwapOption::empty());
        let callback: Arc<MetadataFetchCallback> = Arc::new(Box::new(move |hash| {
            let future = callback(hash);
            Box::pin(async move {
                if future.await {
                    MetadataAdmission::Fetch
                } else {
                    MetadataAdmission::Skip
                }
            })
        }));
        holder.store(Some(callback));
        holder
    }

    fn completion_callback<F>(callback: F) -> Arc<ArcSwapOption<MetadataCompletionCallback>>
    where
        F: Fn(MetadataFetchCompletion) + Send + Sync + 'static,
    {
        let holder = Arc::new(ArcSwapOption::empty());
        let callback: Arc<MetadataCompletionCallback> = Arc::new(Box::new(callback));
        holder.store(Some(callback));
        holder
    }

    fn event(hash: &str, port: u16, discovered_at: Instant) -> HashDiscovered {
        HashDiscovered {
            info_hash: hash.to_string(),
            peer_addr: Some(SocketAddr::from(([127, 0, 0, 1], port))),
            source_node: None,
            source: HashSource::Announce,
            discovered_at,
        }
    }

    fn lookup_request_channel(
        capacity: usize,
    ) -> (JobPeerLookup, mpsc::Receiver<PeerLookupRequest>) {
        let (request_tx, request_rx) = mpsc::channel(capacity);
        (
            JobPeerLookup {
                request_tx,
                info_hash: [3; 20],
                source_nodes: Vec::new(),
                timeout: Duration::from_secs(2),
            },
            request_rx,
        )
    }

    #[test]
    fn runtime_stats_track_queue_outcomes_and_depth() {
        let (_hash_tx, hash_rx) = mpsc::channel(4);
        let stats = DhtRuntimeStats::with_limits(DhtRuntimeLimits {
            metadata_queue: 2,
            ..DhtRuntimeLimits::default()
        });
        let scheduler = MetadataScheduler::new_with_runtime_stats(
            hash_rx,
            Arc::new(RbitFetcher::new(1)),
            MetadataSchedulerLimits {
                queue_size: 2,
                concurrency: 1,
            },
            MetadataSchedulerCallbacks {
                torrent: empty_torrent_callback(),
                fetch_gate: Arc::new(ArcSwapOption::empty()),
                completion: Arc::new(ArcSwapOption::empty()),
            },
            Arc::new(AtomicUsize::new(0)),
            CancellationToken::new(),
            stats.clone(),
        );
        let start = Instant::now();
        let mut queue = PendingHashQueue::new(2, HASH_QUEUE_TTL);
        let mut in_flight = HashMap::new();

        scheduler.enqueue(&mut queue, &mut in_flight, event("old", 1000, start));
        scheduler.enqueue(
            &mut queue,
            &mut in_flight,
            event("old", 1001, start + Duration::from_secs(1)),
        );
        scheduler.enqueue(
            &mut queue,
            &mut in_flight,
            event("middle", 1002, start + Duration::from_secs(2)),
        );
        scheduler.enqueue(
            &mut queue,
            &mut in_flight,
            event("new", 1003, start + Duration::from_secs(3)),
        );
        scheduler.enqueue(
            &mut queue,
            &mut in_flight,
            event(
                "stale",
                1004,
                start
                    .checked_sub(HASH_QUEUE_TTL + Duration::from_secs(1))
                    .unwrap(),
            ),
        );
        scheduler.sync_queue_len(queue.len(), 1);

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.metadata_queue_inserted, 3);
        assert_eq!(snapshot.metadata_queue_deduplicated, 1);
        assert_eq!(snapshot.metadata_queue_evicted, 1);
        assert_eq!(snapshot.metadata_queue_stale, 1);
        assert_eq!(snapshot.metadata_queue_depth, 3);
        assert_eq!(snapshot.metadata_queue_max, 2);
        assert_eq!(snapshot.metadata_in_flight, 1);
    }

    #[test]
    fn queue_deduplicates_hash_and_keeps_bounded_newest_unique_peers() {
        let start = Instant::now();
        let mut queue = PendingHashQueue::new(10, Duration::from_secs(120));
        for offset in 0..=MAX_METADATA_CANDIDATES_PER_HASH as u16 {
            assert_ne!(
                queue.push(
                    event(
                        "hash",
                        1000 + offset,
                        start + Duration::from_secs(offset as u64)
                    ),
                    start + Duration::from_secs(offset as u64),
                ),
                QueuePushKind::Stale
            );
        }
        queue.push(
            event(
                "hash",
                1000 + MAX_METADATA_CANDIDATES_PER_HASH as u16,
                start + Duration::from_secs(100),
            ),
            start + Duration::from_secs(100),
        );

        assert_eq!(queue.len(), 1);
        let entry = queue.remove("hash").unwrap();
        let ports: Vec<_> = entry.peers.iter().map(|peer| peer.addr.port()).collect();
        assert_eq!(
            ports,
            (1001..=1000 + MAX_METADATA_CANDIDATES_PER_HASH as u16)
                .rev()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn coalescing_window_is_fixed_from_first_enqueue() {
        let start = Instant::now();
        let mut queue = PendingHashQueue::new(10, Duration::from_secs(60));
        let in_flight = HashMap::new();
        queue.push(event("hash", 1000, start), start);
        queue.push(
            event("hash", 1001, start + Duration::from_millis(15)),
            start + Duration::from_millis(15),
        );

        assert!(
            queue
                .pop_newest_ready(start + Duration::from_millis(24), &in_flight)
                .is_none()
        );
        let entry = queue
            .pop_newest_ready(start + Duration::from_millis(25), &in_flight)
            .expect("duplicate arrivals must not extend the coalescing deadline");
        assert_eq!(entry.peers.len(), 2);
    }

    #[test]
    fn in_flight_hash_routes_bounded_unique_candidates_to_live_race() {
        let (_hash_tx, hash_rx) = mpsc::channel(4);
        let stats = DhtRuntimeStats::default();
        let scheduler = MetadataScheduler::new_with_runtime_stats(
            hash_rx,
            Arc::new(RbitFetcher::new(1)),
            MetadataSchedulerLimits {
                queue_size: 4,
                concurrency: 1,
            },
            MetadataSchedulerCallbacks {
                torrent: empty_torrent_callback(),
                fetch_gate: Arc::new(ArcSwapOption::empty()),
                completion: Arc::new(ArcSwapOption::empty()),
            },
            Arc::new(AtomicUsize::new(0)),
            CancellationToken::new(),
            stats.clone(),
        );
        let (peer_tx, mut peer_rx) = mpsc::channel(MAX_METADATA_CANDIDATES_PER_HASH);
        let mut in_flight = HashMap::from([(
            "hash".to_string(),
            InFlightState {
                peer_tx,
                scheduled_peers: HashSet::from([SocketAddr::from(([127, 0, 0, 1], 1000))]),
                entry: {
                    let mut queue = PendingHashQueue::new(1, HASH_QUEUE_TTL);
                    queue.push(event("hash", 1000, Instant::now()), Instant::now());
                    queue.remove("hash").unwrap()
                },
            },
        )]);
        let mut queue = PendingHashQueue::new(4, HASH_QUEUE_TTL);
        let now = Instant::now();

        for port in 1001..=1001 + MAX_METADATA_CANDIDATES_PER_HASH as u16 {
            scheduler.enqueue(&mut queue, &mut in_flight, event("hash", port, now));
        }
        scheduler.enqueue(&mut queue, &mut in_flight, event("hash", 1001, now));

        let mut received = Vec::new();
        while let Ok(peer) = peer_rx.try_recv() {
            received.push(peer.addr.port());
        }
        assert_eq!(
            received,
            (1001..1000 + MAX_METADATA_CANDIDATES_PER_HASH as u16).collect::<Vec<_>>()
        );
        assert_eq!(
            in_flight["hash"].scheduled_peers.len(),
            MAX_METADATA_CANDIDATES_PER_HASH
        );
        assert!(queue.is_empty());
        let snapshot = stats.snapshot();
        // Enqueue only forwards candidates; joining is counted after cross-source race deduplication.
        assert_eq!(snapshot.metadata_live_peer_joins, 0);
        assert_eq!(
            snapshot.metadata_queue_deduplicated,
            MAX_METADATA_CANDIDATES_PER_HASH as u64 + 2
        );
    }

    #[tokio::test]
    async fn peer_race_returns_first_success_and_cancels_remaining_fetches() {
        let now = Instant::now();
        let peers = vec![
            PeerCandidate {
                addr: SocketAddr::from(([127, 0, 0, 1], 1000)),
                discovered_at: now,
            },
            PeerCandidate {
                addr: SocketAddr::from(([127, 0, 0, 1], 1001)),
                discovered_at: now,
            },
            PeerCandidate {
                addr: SocketAddr::from(([127, 0, 0, 1], 1002)),
                discovered_at: now,
            },
        ];
        let started = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::new(AtomicBool::new(false));
        let barrier = Arc::new(tokio::sync::Barrier::new(peers.len()));
        let started_for_race = started.clone();
        let cancelled_for_race = cancelled.clone();
        let (_peer_tx, mut peer_rx) = mpsc::channel(MAX_METADATA_CANDIDATES_PER_HASH);
        let stats = DhtRuntimeStats::default();
        let (lookup, mut request_rx) = lookup_request_channel(1);

        let result = race_peer_fetches(
            peers,
            &mut peer_rx,
            Some(lookup),
            move |peer| {
                let started = started_for_race.clone();
                let cancelled = cancelled_for_race.clone();
                let barrier = barrier.clone();
                async move {
                    started.fetch_add(1, Ordering::Relaxed);
                    barrier.wait().await;
                    match peer.addr.port() {
                        1000 => {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            (
                                peer,
                                MetadataFetchOutcome::Fetched((
                                    "winner".to_string(),
                                    1,
                                    Vec::new(),
                                    0,
                                )),
                            )
                        }
                        1001 => {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            (peer, MetadataFetchOutcome::Failed)
                        }
                        _ => {
                            let _signal = DropSignal(cancelled);
                            std::future::pending::<()>().await;
                            unreachable!()
                        }
                    }
                }
            },
            &stats,
        )
        .await
        .expect("one peer should win the race");

        assert_eq!(result.0.addr.port(), 1000);
        assert_eq!(result.1.0, "winner");
        assert!(cancelled.load(Ordering::Relaxed));
        assert!(
            matches!(
                request_rx.try_recv(),
                Err(mpsc::error::TryRecvError::Disconnected)
            ),
            "fast success must drop the delayed lookup without leaving a task behind"
        );
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.metadata_races_started, 1);
        assert_eq!(snapshot.metadata_peer_candidates, 3);
        assert_eq!(snapshot.metadata_peer_canceled, 2);
    }

    #[tokio::test]
    async fn live_peer_joins_running_race_and_cancels_slow_initial_peer() {
        let now = Instant::now();
        let initial = vec![PeerCandidate {
            addr: SocketAddr::from(([127, 0, 0, 1], 1000)),
            discovered_at: now,
        }];
        let live = PeerCandidate {
            addr: SocketAddr::from(([127, 0, 0, 1], 1001)),
            discovered_at: now,
        };
        let (peer_tx, mut peer_rx) = mpsc::channel(MAX_METADATA_CANDIDATES_PER_HASH);
        let initial_started = Arc::new(tokio::sync::Notify::new());
        let initial_started_for_fetch = initial_started.clone();
        let initial_cancelled = Arc::new(AtomicBool::new(false));
        let initial_cancelled_for_fetch = initial_cancelled.clone();
        let stats = DhtRuntimeStats::default();
        let sender = tokio::spawn(async move {
            initial_started.notified().await;
            peer_tx.send(live).await.unwrap();
        });

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            race_peer_fetches(
                initial,
                &mut peer_rx,
                None,
                move |peer| {
                    let initial_started = initial_started_for_fetch.clone();
                    let initial_cancelled = initial_cancelled_for_fetch.clone();
                    async move {
                        if peer.addr.port() == 1000 {
                            let _signal = DropSignal(initial_cancelled);
                            initial_started.notify_one();
                            std::future::pending::<()>().await;
                            unreachable!()
                        }
                        (
                            peer,
                            MetadataFetchOutcome::Fetched((
                                "live-winner".to_string(),
                                1,
                                Vec::new(),
                                0,
                            )),
                        )
                    }
                },
                &stats,
            ),
        )
        .await
        .unwrap()
        .expect("live peer should win");
        sender.await.unwrap();

        assert_eq!(result.0.addr.port(), 1001);
        assert!(initial_cancelled.load(Ordering::Relaxed));
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.metadata_peer_candidates, 2);
        assert_eq!(snapshot.metadata_live_peer_joins, 1);
        assert_eq!(snapshot.metadata_peer_canceled, 1);
    }

    #[tokio::test]
    async fn lookup_and_announce_of_same_peer_count_only_one_live_join() {
        let (_hash_tx, hash_rx) = mpsc::channel(4);
        let stats = DhtRuntimeStats::default();
        let scheduler = MetadataScheduler::new_with_runtime_stats(
            hash_rx,
            Arc::new(RbitFetcher::new(1)),
            MetadataSchedulerLimits {
                queue_size: 4,
                concurrency: 1,
            },
            MetadataSchedulerCallbacks {
                torrent: empty_torrent_callback(),
                fetch_gate: Arc::new(ArcSwapOption::empty()),
                completion: Arc::new(ArcSwapOption::empty()),
            },
            Arc::new(AtomicUsize::new(0)),
            CancellationToken::new(),
            stats.clone(),
        );
        let initial = PeerCandidate {
            addr: SocketAddr::from(([127, 0, 0, 1], 1000)),
            discovered_at: Instant::now(),
        };
        let duplicate_addr = SocketAddr::from(([127, 0, 0, 1], 1001));
        let (peer_tx, mut peer_rx) = mpsc::channel(MAX_METADATA_CANDIDATES_PER_HASH);
        let (lookup, mut request_rx) = lookup_request_channel(1);
        let lookup_peer_started = Arc::new(tokio::sync::Notify::new());
        let started_for_actor = lookup_peer_started.clone();
        let actor = tokio::spawn(async move {
            let request = request_rx.recv().await.unwrap();
            request.peer_tx.send(duplicate_addr).await.unwrap();
            started_for_actor.notified().await;

            // The outer scheduler has not seen this lookup endpoint. Its announce is
            // forwarded, but must not count as another join in the active race.
            let mut in_flight = HashMap::from([(
                "hash".to_string(),
                InFlightState {
                    peer_tx,
                    scheduled_peers: HashSet::from([initial.addr]),
                    entry: {
                        let mut queue = PendingHashQueue::new(1, HASH_QUEUE_TTL);
                        queue.push(event("hash", 1000, Instant::now()), Instant::now());
                        queue.remove("hash").unwrap()
                    },
                },
            )]);
            let mut queue = PendingHashQueue::new(4, HASH_QUEUE_TTL);
            scheduler.enqueue(
                &mut queue,
                &mut in_flight,
                event("hash", 1001, Instant::now()),
            );
            assert!(in_flight["hash"].scheduled_peers.contains(&duplicate_addr));
        });
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            race_peer_fetches(
                vec![initial],
                &mut peer_rx,
                Some(lookup),
                move |peer| {
                    let started = lookup_peer_started.clone();
                    async move {
                        if peer.addr == duplicate_addr {
                            started.notify_one();
                        }
                        (peer, MetadataFetchOutcome::Failed)
                    }
                },
                &stats,
            ),
        )
        .await
        .unwrap();
        actor.await.unwrap();
        assert!(result.is_none());
        assert_eq!(stats.snapshot().metadata_peer_candidates, 2);
        assert_eq!(stats.snapshot().metadata_live_peer_joins, 1);
    }

    #[tokio::test]
    async fn lookup_peer_arriving_after_idle_grace_can_win_after_initial_peer_failed() {
        let initial = PeerCandidate {
            addr: SocketAddr::from(([127, 0, 0, 1], 1000)),
            discovered_at: Instant::now(),
        };
        let (_peer_tx, mut peer_rx) = mpsc::channel(12);
        let (lookup, mut request_rx) = lookup_request_channel(1);
        let actor = tokio::spawn(async move {
            let request = request_rx.recv().await.unwrap();
            tokio::time::sleep(Duration::from_millis(400)).await;
            request.peer_tx.send(initial.addr).await.unwrap();
            request
                .peer_tx
                .send(SocketAddr::from(([127, 0, 0, 1], 1001)))
                .await
                .unwrap();
            // Closing immediately must not make completion overtake the buffered winner.
        });
        let stats = DhtRuntimeStats::default();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            race_peer_fetches(
                vec![initial],
                &mut peer_rx,
                Some(lookup),
                |peer| async move {
                    let outcome = if peer.addr.port() == 1000 {
                        MetadataFetchOutcome::SkippedCached
                    } else {
                        MetadataFetchOutcome::Fetched(("late-winner".to_string(), 1, Vec::new(), 0))
                    };
                    (peer, outcome)
                },
                &stats,
            ),
        )
        .await
        .unwrap()
        .expect("pending lookup must survive the 300 ms idle grace");
        actor.await.unwrap();
        assert_eq!(result.0.addr.port(), 1001);
        assert_eq!(
            stats.snapshot().metadata_peer_candidates,
            2,
            "duplicate lookup peer must not consume a slot"
        );
    }

    #[tokio::test]
    async fn twelve_cached_candidates_allow_a_late_lookup_winner() {
        let (_peer_tx, mut peer_rx) = mpsc::channel(MAX_METADATA_CANDIDATES_PER_HASH);
        let (lookup, mut request_rx) = lookup_request_channel(1);
        let actor = tokio::spawn(async move {
            let request = request_rx.recv().await.unwrap();
            tokio::time::sleep(Duration::from_millis(400)).await;
            request
                .peer_tx
                .send(SocketAddr::from(([127, 0, 0, 1], 2000)))
                .await
                .unwrap();
        });
        let peers = (1000..1012)
            .map(|port| PeerCandidate {
                addr: SocketAddr::from(([127, 0, 0, 1], port)),
                discovered_at: Instant::now(),
            })
            .collect();
        let actual = Arc::new(AtomicUsize::new(0));
        let recorded = actual.clone();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            race_peer_fetches(
                peers,
                &mut peer_rx,
                Some(lookup),
                move |peer| {
                    let recorded = recorded.clone();
                    async move {
                        let outcome = if peer.addr.port() == 2000 {
                            recorded.fetch_add(1, Ordering::Relaxed);
                            MetadataFetchOutcome::Fetched(("late".to_string(), 1, vec![], 0))
                        } else {
                            MetadataFetchOutcome::SkippedCached
                        };
                        (peer, outcome)
                    }
                },
                &DhtRuntimeStats::default(),
            ),
        )
        .await
        .unwrap();
        actor.await.unwrap();
        assert_eq!(result.unwrap().0.addr.port(), 2000);
        assert_eq!(actual.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn cached_refills_preserve_actual_attempt_and_candidate_scan_bounds() {
        for all_cached in [false, true] {
            let (peer_tx, mut peer_rx) = mpsc::channel(MAX_METADATA_CANDIDATES_PER_HASH);
            drop(peer_tx);
            // Inputs exceed the scan bound; even an all-cached stream must stop at 64.
            let peers = (1000..1100)
                .map(|port| PeerCandidate {
                    addr: SocketAddr::from(([127, 0, 0, 1], port)),
                    discovered_at: Instant::now(),
                })
                .collect();
            let actual = Arc::new(AtomicUsize::new(0));
            let scanned = Arc::new(AtomicUsize::new(0));
            let actual_for_fetch = actual.clone();
            let scanned_for_fetch = scanned.clone();
            let stats = DhtRuntimeStats::default();
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                race_peer_fetches(
                    peers,
                    &mut peer_rx,
                    None,
                    move |peer| {
                        let actual = actual_for_fetch.clone();
                        let scanned = scanned_for_fetch.clone();
                        async move {
                            scanned.fetch_add(1, Ordering::Relaxed);
                            let outcome = if all_cached || peer.addr.port() < 1012 {
                                MetadataFetchOutcome::SkippedCached
                            } else {
                                actual.fetch_add(1, Ordering::Relaxed);
                                tokio::task::yield_now().await;
                                MetadataFetchOutcome::Failed
                            };
                            (peer, outcome)
                        }
                    },
                    &stats,
                ),
            )
            .await
            .unwrap();
            assert!(result.is_none());
            assert_eq!(
                actual.load(Ordering::Relaxed),
                if all_cached {
                    0
                } else {
                    MAX_METADATA_PEERS_PER_HASH
                }
            );
            assert_eq!(
                scanned.load(Ordering::Relaxed),
                if all_cached {
                    MAX_METADATA_CANDIDATES_PER_HASH
                } else {
                    MAX_METADATA_PEERS_PER_HASH * 2
                }
            );
            assert_eq!(
                stats.snapshot().metadata_peer_candidates,
                scanned.load(Ordering::Relaxed) as u64
            );
        }
    }

    #[tokio::test]
    async fn full_lookup_request_queue_ends_wait_promptly() {
        let (_peer_tx, mut peer_rx) = mpsc::channel(12);
        let (lookup, _request_rx) = lookup_request_channel(1);
        let (peer_tx, _result_rx) = mpsc::channel(12);
        lookup
            .request_tx
            .try_send(PeerLookupRequest {
                info_hash: [4; 20],
                peer_tx,
                source_nodes: Vec::new(),
            })
            .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            race_peer_fetches(
                Vec::new(),
                &mut peer_rx,
                Some(lookup),
                |peer| async move { (peer, MetadataFetchOutcome::Failed) },
                &DhtRuntimeStats::default(),
            ),
        )
        .await
        .expect("rejected lookup must not wait for its full deadline");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn finished_lookup_with_no_peers_ends_wait_promptly() {
        let (peer_tx, mut peer_rx) = mpsc::channel(12);
        drop(peer_tx);
        let (lookup, mut request_rx) = lookup_request_channel(1);
        let actor = tokio::spawn(async move {
            drop(request_rx.recv().await.unwrap());
        });
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            race_peer_fetches(
                Vec::new(),
                &mut peer_rx,
                Some(lookup),
                |peer| async move { (peer, MetadataFetchOutcome::Failed) },
                &DhtRuntimeStats::default(),
            ),
        )
        .await
        .unwrap();
        actor.await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn stalled_lookup_is_bounded_and_closes_its_result_stream() {
        let (peer_tx, mut peer_rx) = mpsc::channel(12);
        drop(peer_tx);
        let (lookup, mut request_rx) = lookup_request_channel(1);
        let actor = tokio::spawn(async move {
            let request = request_rx.recv().await.unwrap();
            request.peer_tx.closed().await;
        });
        let result = tokio::time::timeout(
            Duration::from_secs(4),
            race_peer_fetches(
                Vec::new(),
                &mut peer_rx,
                Some(lookup),
                |peer| async move { (peer, MetadataFetchOutcome::Failed) },
                &DhtRuntimeStats::default(),
            ),
        )
        .await
        .expect("a stalled actor must not retain the job indefinitely");
        assert!(result.is_none());
        actor.await.unwrap();
    }

    #[tokio::test]
    async fn metadata_success_cancels_a_lookup_already_in_progress() {
        let (_peer_tx, mut peer_rx) = mpsc::channel(12);
        let (lookup, mut request_rx) = lookup_request_channel(1);
        let actor_started = Arc::new(tokio::sync::Notify::new());
        let started_for_actor = actor_started.clone();
        let actor = tokio::spawn(async move {
            let request = request_rx.recv().await.unwrap();
            started_for_actor.notify_one();
            request.peer_tx.closed().await;
        });
        let initial = PeerCandidate {
            addr: SocketAddr::from(([127, 0, 0, 1], 1000)),
            discovered_at: Instant::now(),
        };
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            race_peer_fetches(
                vec![initial],
                &mut peer_rx,
                Some(lookup),
                move |peer| {
                    let actor_started = actor_started.clone();
                    async move {
                        actor_started.notified().await;
                        (
                            peer,
                            MetadataFetchOutcome::Fetched(("winner".to_string(), 1, Vec::new(), 0)),
                        )
                    }
                },
                &DhtRuntimeStats::default(),
            ),
        )
        .await
        .unwrap();
        assert!(result.is_some());
        tokio::time::timeout(Duration::from_secs(1), actor)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn full_queue_evicts_oldest_for_newer_hash() {
        let start = Instant::now();
        let mut queue = PendingHashQueue::new(2, Duration::from_secs(60));
        queue.push(event("old", 1000, start), start);
        queue.push(
            event("middle", 1001, start + Duration::from_secs(1)),
            start + Duration::from_secs(1),
        );
        let result = queue.push(
            event("new", 1002, start + Duration::from_secs(2)),
            start + Duration::from_secs(2),
        );

        assert_eq!(result, QueuePushKind::EvictedOldest);
        assert!(!queue.contains("old"));
        assert!(queue.contains("middle"));
        assert!(queue.contains("new"));
    }

    #[test]
    fn queue_expires_stale_hashes_and_pops_newest_first() {
        let start = Instant::now();
        let mut queue = PendingHashQueue::new(4, Duration::from_secs(10));
        queue.push(event("older", 1000, start), start);
        queue.push(
            event("newer", 1001, start + Duration::from_secs(1)),
            start + Duration::from_secs(1),
        );

        let in_flight = HashMap::new();
        assert!(
            queue
                .pop_newest_ready(start + Duration::from_millis(24), &in_flight)
                .is_none()
        );
        let newest = queue
            .pop_newest_ready(start + Duration::from_secs(2), &in_flight)
            .unwrap();
        assert_eq!(newest.info_hash, "newer");
        assert_eq!(queue.expire(start + Duration::from_secs(11)), 1);
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn gate_rejection_does_not_emit_completion() {
        let (hash_tx, hash_rx) = mpsc::channel(4);
        let gate_calls = Arc::new(AtomicUsize::new(0));
        let gate_calls_for_callback = gate_calls.clone();
        let gate = fetch_gate(move |_| {
            gate_calls_for_callback.fetch_add(1, Ordering::Relaxed);
            async { false }
        });
        let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
        let completion = completion_callback(move |result| {
            let _ = completion_tx.send(result);
        });
        let shutdown = CancellationToken::new();
        let scheduler = MetadataScheduler::new(
            hash_rx,
            Arc::new(RbitFetcher::new(1)),
            MetadataSchedulerLimits {
                queue_size: 4,
                concurrency: 1,
            },
            MetadataSchedulerCallbacks {
                torrent: empty_torrent_callback(),
                fetch_gate: gate,
                completion,
            },
            Arc::new(AtomicUsize::new(0)),
            shutdown.clone(),
        );
        let scheduler_task = tokio::spawn(scheduler.run());
        hash_tx
            .send(event("bad", 1000, Instant::now()))
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(1), async {
            while gate_calls.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), completion_rx.recv())
                .await
                .is_err()
        );

        shutdown.cancel();
        scheduler_task.await.unwrap();
    }

    #[tokio::test]
    async fn admitted_failure_emits_one_completion() {
        let (hash_tx, hash_rx) = mpsc::channel(4);
        let gate = fetch_gate(|_| async { true });
        let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
        let completion = completion_callback(move |result| {
            let _ = completion_tx.send(result);
        });
        let shutdown = CancellationToken::new();
        let scheduler = MetadataScheduler::new(
            hash_rx,
            Arc::new(RbitFetcher::new(1)),
            MetadataSchedulerLimits {
                queue_size: 4,
                concurrency: 1,
            },
            MetadataSchedulerCallbacks {
                torrent: empty_torrent_callback(),
                fetch_gate: gate,
                completion,
            },
            Arc::new(AtomicUsize::new(0)),
            shutdown.clone(),
        );
        let scheduler_task = tokio::spawn(scheduler.run());
        hash_tx
            .send(event("bad", 1000, Instant::now()))
            .await
            .unwrap();

        let result = tokio::time::timeout(Duration::from_secs(1), completion_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.info_hash, "bad");
        assert_eq!(result.status, MetadataFetchCompletionStatus::FetchFailed);
        assert_eq!(result.attempts, 0);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), completion_rx.recv())
                .await
                .is_err()
        );

        shutdown.cancel();
        scheduler_task.await.unwrap();
    }

    #[test]
    fn retries_have_fixed_lifetime_bounded_history_and_exponential_failure_budget() {
        let now = Instant::now();
        let policy = MetadataOptions {
            retry_initial_delay_secs: 1,
            retry_max_delay_secs: 3,
            retry_max_attempts: 3,
            retry_history_ttl_secs: 20,
            retry_history_capacity: 1,
            ..MetadataOptions::default()
        };
        let mut history = RetryHistory::default();
        let first = history
            .record("h", now, true, Duration::ZERO, &policy)
            .unwrap();
        assert_eq!(first.failures, 1);
        assert_eq!(first.next_due, now + Duration::from_secs(1));
        let deferred = history
            .record(
                "h",
                now + Duration::from_secs(1),
                false,
                Duration::from_secs(5),
                &policy,
            )
            .unwrap();
        assert_eq!(
            deferred.failures, 1,
            "lease checks are not download attempts"
        );
        assert_eq!(deferred.expires_at, first.expires_at);
        let second = history
            .record(
                "h",
                now + Duration::from_secs(6),
                true,
                Duration::ZERO,
                &policy,
            )
            .unwrap();
        assert_eq!(second.next_due, now + Duration::from_secs(8));
        let third = history
            .record(
                "h",
                now + Duration::from_secs(8),
                true,
                Duration::ZERO,
                &policy,
            )
            .unwrap();
        assert_eq!(third.next_due, now + Duration::from_secs(11));
        assert_eq!(third.failures, policy.retry_max_attempts);
        assert!(
            history
                .record("other", now, true, Duration::ZERO, &policy)
                .is_none()
        );
        history.expire(now + Duration::from_secs(20));
        assert!(history.entries.is_empty());
        assert!(history.expiry.is_empty());
    }

    #[test]
    fn deferred_candidates_are_preserved_without_sliding_due_time_or_unbounded_indexes() {
        let now = Instant::now();
        let mut queue = PendingHashQueue::new(8, HASH_QUEUE_TTL);
        queue.push(event("h", 1000, now), now);
        let entry = queue.remove("h").unwrap();
        let record = RetryRecord {
            failures: 1,
            extra_attempts: 0,
            next_due: now + Duration::from_secs(2),
            expires_at: now + Duration::from_secs(10),
        };
        assert!(queue.defer(entry, record));
        for _ in 0..1_000 {
            queue.push(
                event("h", 1001, now + Duration::from_secs(1)),
                now + Duration::from_secs(1),
            );
        }
        assert_eq!(queue.waiting.len(), 1);
        assert_eq!(queue.expiry.len(), 1);
        assert_eq!(queue.retry_expiry.len(), 1);
        assert_eq!(
            queue.pressure_len(),
            0,
            "cooling work must not slow discovery"
        );
        assert!(
            queue
                .pop_newest_ready(now + Duration::from_secs(1), &HashMap::new())
                .is_none()
        );
        let retained = queue
            .pop_newest_ready(record.next_due, &HashMap::new())
            .unwrap();
        assert_eq!(retained.peers.len(), 2);
        assert_eq!(retained.peers[0].addr.port(), 1001);
        assert_eq!(retained.expires_at, record.expires_at);
        assert!(
            queue.expiry.is_empty() && queue.waiting.is_empty() && queue.retry_expiry.is_empty()
        );
    }

    #[test]
    fn fresh_and_due_retry_work_have_bounded_fairness_and_cold_retries_reserve_no_pressure() {
        let now = Instant::now();
        let mut queue = PendingHashQueue::new(12, HASH_QUEUE_TTL);
        queue.push(event("retry", 1000, now), now);
        let retry = queue.remove("retry").unwrap();
        assert!(queue.defer(
            retry,
            RetryRecord {
                failures: 1,
                extra_attempts: 0,
                next_due: now + Duration::from_secs(1),
                expires_at: now + Duration::from_secs(10)
            }
        ));
        for offset in 0..6 {
            queue.push(event(&format!("fresh-{offset}"), 1100 + offset, now), now);
        }
        let kinds: Vec<_> = (0..4)
            .map(|_| {
                queue
                    .pop_newest_ready(now + Duration::from_secs(1), &HashMap::new())
                    .unwrap()
                    .is_retry
            })
            .collect();
        assert_eq!(kinds, [false, false, false, true]);
    }

    #[tokio::test]
    async fn hash_only_job_uses_source_for_lookup_without_connecting_to_its_udp_address() {
        let source = NodeTuple {
            id: [8; 20],
            addr: SocketAddr::from(([127, 0, 0, 1], 9999)),
        };
        let (request_tx, mut request_rx) = mpsc::channel(1);
        let actor = tokio::spawn(async move {
            let request = request_rx.recv().await.unwrap();
            let request: PeerLookupRequest = request;
            assert_eq!(request.source_nodes[0].addr, source.addr);
            // An empty lookup must never turn its seed endpoint into a TCP candidate.
        });
        let (peer_tx, peer_rx) = mpsc::channel(1);
        drop(peer_tx);
        let stats = DhtRuntimeStats::default();
        let result = MetadataScheduler::process_hash(
            MetadataJob {
                info_hash: hex::encode([3; 20]),
                peers: Vec::new(),
                peer_rx,
                source_nodes: vec![source],
                source: HashSource::GetPeers,
            },
            &Arc::new(RbitFetcher::new(1)),
            &empty_torrent_callback(),
            &fetch_gate(|_| async { true }),
            &stats,
            Some(request_tx),
            Duration::from_secs(1),
        )
        .await;
        actor.await.unwrap();
        assert_eq!(result.attempts, 0);
        assert_eq!(stats.snapshot().metadata_peer_attempts, 0);
        assert_eq!(
            stats.yield_stats().snapshot()["metadata_hash_only_started"],
            1
        );
    }

    #[tokio::test]
    async fn one_new_peer_during_failure_cooldown_is_fetched_without_another_announce() {
        use bytes::Bytes;
        use rbit::{ExtensionHandshake, Message, MetadataMessage, PeerConnection};
        use sha1::{Digest, Sha1};
        use tokio::net::TcpListener;
        let metadata = Bytes::from_static(b"d6:lengthi1e4:name1:xe");
        let hash: [u8; 20] = Sha1::digest(&metadata).into();
        let failed_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let failed_addr = failed_listener.local_addr().unwrap();
        let good_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let good_addr = good_listener.local_addr().unwrap();
        drop(failed_listener);
        let admissions = Arc::new(AtomicUsize::new(0));
        let seen_admissions = admissions.clone();
        let gate: Arc<ArcSwapOption<MetadataFetchCallback>> = Arc::new(ArcSwapOption::empty());
        gate.store(Some(Arc::new(Box::new(move |_| {
            let number = seen_admissions.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                if number == 1 {
                    MetadataAdmission::RetryAfter(Duration::from_millis(200))
                } else {
                    MetadataAdmission::Fetch
                }
            })
        }))));
        let check_admissions = admissions.clone();
        let peer_actor = tokio::spawn(async move {
            let (stream, _) = good_listener.accept().await.unwrap();
            assert!(
                check_admissions.load(Ordering::Relaxed) >= 3,
                "new peers must still pass admission after cooldown"
            );
            let mut conn = PeerConnection::accept(stream, hash, [9; 20]).await.unwrap();
            assert!(matches!(
                conn.receive().await.unwrap(),
                Message::Extended { id: 0, .. }
            ));
            let mut handshake = ExtensionHandshake::with_extensions(&[("ut_metadata", 3)]);
            handshake.metadata_size = Some(metadata.len() as i64);
            conn.send(Message::Extended {
                id: 0,
                payload: handshake.encode().unwrap(),
            })
            .await
            .unwrap();
            assert!(matches!(
                conn.receive().await.unwrap(),
                Message::Extended { id: 3, .. }
            ));
            conn.send(Message::Extended {
                id: 1,
                payload: MetadataMessage::data(0, metadata.len() as u32, metadata)
                    .encode()
                    .unwrap(),
            })
            .await
            .unwrap();
        });
        let torrent: Arc<ArcSwapOption<TorrentAckCallback>> = Arc::new(ArcSwapOption::empty());
        torrent.store(Some(Arc::new(Box::new(|_| true))));
        let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
        let (hash_tx, hash_rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let stats = DhtRuntimeStats::default();
        let scheduler = MetadataScheduler::new_with_runtime(
            hash_rx,
            Arc::new(RbitFetcher::new(1)),
            MetadataSchedulerLimits {
                queue_size: 32,
                concurrency: 1,
            },
            MetadataSchedulerCallbacks {
                torrent,
                fetch_gate: gate,
                completion: completion_callback(move |result| {
                    let _ = completion_tx.send(result);
                }),
            },
            Arc::new(AtomicUsize::new(0)),
            shutdown.clone(),
            MetadataSchedulerRuntime {
                stats: stats.clone(),
                peer_lookup_tx: None,
                lookup_timeout: Duration::from_secs(1),
                metadata_options: MetadataOptions {
                    retry_initial_delay_secs: 0,
                    retry_max_delay_secs: 0,
                    ..MetadataOptions::default()
                },
            },
        );
        let task = tokio::spawn(scheduler.run());
        hash_tx
            .send(HashDiscovered {
                info_hash: hex::encode(hash),
                peer_addr: Some(failed_addr),
                source_node: None,
                source: HashSource::Announce,
                discovered_at: Instant::now(),
            })
            .await
            .unwrap();
        let first = tokio::time::timeout(Duration::from_secs(2), completion_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.status, MetadataFetchCompletionStatus::FetchFailed);
        hash_tx
            .send(HashDiscovered {
                info_hash: hex::encode(hash),
                peer_addr: Some(good_addr),
                source_node: None,
                source: HashSource::Announce,
                discovered_at: Instant::now(),
            })
            .await
            .unwrap();
        let second = tokio::time::timeout(Duration::from_secs(2), completion_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.status, MetadataFetchCompletionStatus::Accepted);
        assert_eq!(admissions.load(Ordering::Relaxed), 3);
        assert_eq!(
            first.attempts + second.attempts,
            2,
            "the cached failed endpoint must not cause another TCP attempt"
        );
        assert_eq!(stats.yield_stats().snapshot()["metadata_retry_deferred"], 1);
        shutdown.cancel();
        task.await.unwrap();
        peer_actor.await.unwrap();
    }

    #[test]
    fn exhausted_history_allows_only_two_new_candidate_chances_and_never_old_duplicates() {
        let now = Instant::now();
        let policy = MetadataOptions {
            retry_max_attempts: 1,
            ..MetadataOptions::default()
        };
        let mut history = RetryHistory::default();
        history
            .record("h", now, true, Duration::ZERO, &policy)
            .unwrap();
        let mut queue = PendingHashQueue::new(8, HASH_QUEUE_TTL);
        queue.push(event("h", 1000, now), now);
        history.remember(&queue.remove("h").unwrap());
        for _ in 0..100 {
            let record = history
                .observe_event(&event("h", 1000, now), now, &policy)
                .unwrap();
            assert_eq!(record.extra_attempts, 0);
            assert_eq!(record.failures, record.attempt_limit(&policy));
        }
        for (offset, expected_extra) in [(1, 1), (2, 2), (3, 2)] {
            let record = history
                .observe_event(&event("h", 1000 + offset, now), now, &policy)
                .unwrap();
            assert_eq!(record.extra_attempts, expected_extra);
            if offset <= 2 {
                assert!(record.failures < record.attempt_limit(&policy));
                history
                    .record("h", now, true, Duration::ZERO, &policy)
                    .unwrap();
            } else {
                assert_eq!(record.failures, record.attempt_limit(&policy));
            }
        }
    }

    #[test]
    fn retry_extreme_durations_are_clamped_without_panics() {
        let now = Instant::now();
        let policy = MetadataOptions {
            retry_initial_delay_secs: u64::MAX,
            retry_max_delay_secs: u64::MAX,
            retry_history_ttl_secs: u64::MAX,
            ..MetadataOptions::default()
        };
        let mut history = RetryHistory::default();
        let record = history
            .record("h", now, true, Duration::ZERO, &policy)
            .unwrap();
        assert_eq!(record.next_due.duration_since(now), MAX_RETRY_LIFETIME);
        assert_eq!(record.expires_at.duration_since(now), MAX_RETRY_LIFETIME);
        let deferred = history
            .record("h", now, false, Duration::MAX, &policy)
            .unwrap();
        assert_eq!(deferred.next_due.duration_since(now), MAX_RETRY_LIFETIME);
    }

    #[tokio::test]
    async fn panicking_admission_releases_running_entry_and_disabled_retry_keeps_new_announces_eligible()
     {
        let admissions = Arc::new(AtomicUsize::new(0));
        let called = admissions.clone();
        let gate = fetch_gate(move |_| {
            let call = called.fetch_add(1, Ordering::Relaxed);
            async move {
                assert_ne!(call, 0, "test admission panic");
                false
            }
        });
        let (hash_tx, hash_rx) = mpsc::channel(8);
        let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
        let shutdown = CancellationToken::new();
        let stats = DhtRuntimeStats::default();
        let scheduler = MetadataScheduler::new_with_runtime(
            hash_rx,
            Arc::new(RbitFetcher::new(1)),
            MetadataSchedulerLimits {
                queue_size: 8,
                concurrency: 1,
            },
            MetadataSchedulerCallbacks {
                torrent: empty_torrent_callback(),
                fetch_gate: gate,
                completion: completion_callback(move |result| {
                    let _ = completion_tx.send(result);
                }),
            },
            Arc::new(AtomicUsize::new(0)),
            shutdown.clone(),
            MetadataSchedulerRuntime {
                stats: stats.clone(),
                peer_lookup_tx: None,
                lookup_timeout: Duration::from_secs(1),
                metadata_options: MetadataOptions {
                    retry_max_attempts: 0,
                    ..MetadataOptions::default()
                },
            },
        );
        let task = tokio::spawn(scheduler.run());
        hash_tx
            .send(event("h", 1000, Instant::now()))
            .await
            .unwrap();
        let completion = tokio::time::timeout(Duration::from_secs(1), completion_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            completion.status,
            MetadataFetchCompletionStatus::FetchFailed
        );
        hash_tx
            .send(event("h", 1001, Instant::now()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while admissions.load(Ordering::Relaxed) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            stats.yield_stats().snapshot()["metadata_retry_scheduled"],
            0
        );
        shutdown.cancel();
        task.await.unwrap();
        assert_eq!(stats.snapshot().metadata_in_flight, 0);
    }
}
