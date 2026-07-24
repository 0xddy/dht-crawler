use crate::metadata::{FetchedMetadata, MetadataFetchOutcome, RbitFetcher};
#[cfg(test)]
use crate::runtime_stats::DhtRuntimeLimits;
use crate::runtime_stats::DhtRuntimeStats;
use crate::server::HashDiscovered;
use crate::types::{MetadataFetchCompletion, MetadataFetchCompletionStatus, TorrentInfo};
use arc_swap::ArcSwapOption;
#[cfg(feature = "metrics")]
use metrics::{counter, gauge, histogram};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::net::SocketAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// Maximum distinct Peer endpoints retained and raced for one InfoHash.
pub const MAX_METADATA_PEERS_PER_HASH: usize = 12;
const HASH_QUEUE_TTL: Duration = Duration::from_secs(60);
const PEER_COALESCE_WINDOW: Duration = Duration::from_millis(25);
const LIVE_PEER_IDLE_GRACE: Duration = Duration::from_millis(300);
const ACTIVE_PEER_LOOKUP_DELAY: Duration = Duration::from_millis(100);
const DISPATCH_TICK: Duration = Duration::from_millis(25);

/// Callback returning whether the application accepted a downloaded torrent.
pub type TorrentAckCallback = Box<dyn Fn(TorrentInfo) -> bool + Send + Sync + 'static>;
/// Callback invoked once when an admitted Metadata job reaches a terminal state.
pub type MetadataCompletionCallback = Box<dyn Fn(MetadataFetchCompletion) + Send + Sync + 'static>;
/// Asynchronous InfoHash admission callback.
pub type MetadataFetchCallback = Box<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
        + Send
        + Sync
        + 'static,
>;

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
    pub(crate) peer_lookup_tx: Option<mpsc::Sender<[u8; 20]>>,
}

#[derive(Debug, Clone, Copy)]
struct PeerCandidate {
    addr: SocketAddr,
    discovered_at: Instant,
}

#[derive(Debug)]
struct QueuedHash {
    info_hash: String,
    peers: Vec<PeerCandidate>,
    queued_at: Instant,
    ready_at: Instant,
    latest_at: Instant,
    order_key: (Instant, u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueuePushKind {
    Inserted,
    Updated,
    EvictedOldest,
    Stale,
}

#[derive(Debug)]
struct PendingHashQueue {
    capacity: usize,
    ttl: Duration,
    entries: HashMap<String, QueuedHash>,
    order: BTreeMap<(Instant, u64), String>,
    sequence: u64,
}

impl PendingHashQueue {
    fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            capacity: capacity.max(1),
            ttl,
            entries: HashMap::new(),
            order: BTreeMap::new(),
            sequence: 0,
        }
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(test)]
    fn contains(&self, info_hash: &str) -> bool {
        self.entries.contains_key(info_hash)
    }

    fn next_order_key(&mut self, at: Instant) -> (Instant, u64) {
        self.sequence = self.sequence.wrapping_add(1);
        (at, self.sequence)
    }

    fn push(&mut self, event: HashDiscovered, now: Instant) -> QueuePushKind {
        if now
            .checked_duration_since(event.discovered_at)
            .unwrap_or_default()
            > self.ttl
        {
            return QueuePushKind::Stale;
        }

        if let Some(mut entry) = self.entries.remove(&event.info_hash) {
            self.order.remove(&entry.order_key);
            let discovered_at = entry
                .peers
                .iter()
                .find(|peer| peer.addr == event.peer_addr)
                .map(|peer| peer.discovered_at.max(event.discovered_at))
                .unwrap_or(event.discovered_at);
            entry.peers.retain(|peer| peer.addr != event.peer_addr);
            entry.peers.push(PeerCandidate {
                addr: event.peer_addr,
                discovered_at,
            });
            entry
                .peers
                .sort_unstable_by(|left, right| right.discovered_at.cmp(&left.discovered_at));
            entry.peers.truncate(MAX_METADATA_PEERS_PER_HASH);
            entry.latest_at = entry.latest_at.max(event.discovered_at);
            entry.order_key = self.next_order_key(entry.latest_at);
            self.order.insert(entry.order_key, entry.info_hash.clone());
            self.entries.insert(entry.info_hash.clone(), entry);
            return QueuePushKind::Updated;
        }

        let mut result = QueuePushKind::Inserted;
        if self.entries.len() >= self.capacity {
            let Some((&oldest_key, oldest_hash)) = self.order.first_key_value() else {
                return QueuePushKind::Stale;
            };
            if event.discovered_at <= oldest_key.0 {
                return QueuePushKind::Stale;
            }
            let oldest_hash = oldest_hash.clone();
            self.order.remove(&oldest_key);
            self.entries.remove(&oldest_hash);
            result = QueuePushKind::EvictedOldest;
        }

        let order_key = self.next_order_key(event.discovered_at);
        let info_hash = event.info_hash;
        self.order.insert(order_key, info_hash.clone());
        self.entries.insert(
            info_hash.clone(),
            QueuedHash {
                info_hash,
                peers: vec![PeerCandidate {
                    addr: event.peer_addr,
                    discovered_at: event.discovered_at,
                }],
                queued_at: now,
                ready_at: now + PEER_COALESCE_WINDOW,
                latest_at: event.discovered_at,
                order_key,
            },
        );
        result
    }

    fn remove(&mut self, info_hash: &str) -> Option<QueuedHash> {
        let entry = self.entries.remove(info_hash)?;
        self.order.remove(&entry.order_key);
        Some(entry)
    }

    fn pop_newest_ready(
        &mut self,
        now: Instant,
        in_flight: &HashMap<String, InFlightState>,
    ) -> Option<QueuedHash> {
        let info_hash = self.order.iter().rev().find_map(|(_, hash)| {
            let entry = self.entries.get(hash)?;
            (!in_flight.contains_key(hash) && entry.ready_at <= now).then(|| hash.clone())
        })?;
        self.remove(&info_hash)
    }

    fn expire(&mut self, now: Instant) -> usize {
        let mut expired = 0;
        loop {
            let Some((&oldest_key, oldest_hash)) = self.order.first_key_value() else {
                break;
            };
            if now.checked_duration_since(oldest_key.0).unwrap_or_default() <= self.ttl {
                break;
            }
            let oldest_hash = oldest_hash.clone();
            self.order.remove(&oldest_key);
            self.entries.remove(&oldest_hash);
            expired += 1;
        }
        expired
    }
}

#[derive(Debug)]
struct MetadataJob {
    info_hash: String,
    peers: Vec<PeerCandidate>,
    peer_rx: mpsc::Receiver<PeerCandidate>,
}

#[derive(Debug)]
struct InFlightState {
    peer_tx: mpsc::Sender<PeerCandidate>,
    scheduled_peers: HashSet<SocketAddr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobOutcome {
    GateRejected,
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
    peer_lookup_tx: Option<mpsc::Sender<[u8; 20]>>,
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
        let mut maintenance = tokio::time::interval(Duration::from_secs(1));
        maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut dispatch_tick = tokio::time::interval(DISPATCH_TICK);
        dispatch_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut stats_interval = tokio::time::interval(Duration::from_secs(60));
        stats_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut input_closed = false;

        loop {
            self.dispatch_ready(&mut queue, &mut in_flight, &mut tasks);
            self.sync_queue_len(queue.len(), in_flight.len());

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
                result = tasks.join_next(), if !tasks.is_empty() => {
                    match result {
                        Some(Ok(result)) => {
                            self.handle_job_result(result, &mut queue, &mut in_flight);
                        }
                        Some(Err(error)) => {
                            #[cfg(feature = "metrics")]
                            counter!("dht_metadata_worker_join_error_total").increment(1);
                            log::warn!("Metadata worker task failed: {error}");
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
            let kind = if now
                .checked_duration_since(hash.discovered_at)
                .unwrap_or_default()
                > HASH_QUEUE_TTL
            {
                QueuePushKind::Stale
            } else if state.scheduled_peers.contains(&hash.peer_addr)
                || state.scheduled_peers.len() >= MAX_METADATA_PEERS_PER_HASH
            {
                QueuePushKind::Updated
            } else {
                let peer = PeerCandidate {
                    addr: hash.peer_addr,
                    discovered_at: hash.discovered_at,
                };
                match state.peer_tx.try_send(peer) {
                    Ok(()) => {
                        state.scheduled_peers.insert(hash.peer_addr);
                        self.runtime_stats.metadata_live_peer_join();
                        #[cfg(feature = "metrics")]
                        counter!("dht_metadata_live_peer_joins_total").increment(1);
                        QueuePushKind::Updated
                    }
                    Err(_) => QueuePushKind::Updated,
                }
            };
            self.record_queue_push(kind);
            return;
        }

        let kind = queue.push(hash, now);
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
            QueuePushKind::EvictedOldest => {
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
    ) {
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
                now.checked_duration_since(peer.discovered_at)
                    .unwrap_or_default()
                    <= HASH_QUEUE_TTL
            });
            entry.peers.truncate(MAX_METADATA_PEERS_PER_HASH);
            if entry.peers.is_empty() {
                self.total_dropped.fetch_add(1, Ordering::Relaxed);
                self.runtime_stats.metadata_queue_stale(1);
                continue;
            }
            let (peer_tx, peer_rx) = mpsc::channel(MAX_METADATA_PEERS_PER_HASH);
            let state = InFlightState {
                peer_tx,
                scheduled_peers: entry.peers.iter().map(|peer| peer.addr).collect(),
            };
            in_flight.insert(entry.info_hash.clone(), state);
            self.spawn_job(entry, peer_rx, tasks);
        }
    }

    fn spawn_job(
        &self,
        entry: QueuedHash,
        peer_rx: mpsc::Receiver<PeerCandidate>,
        tasks: &mut JoinSet<JobResult>,
    ) {
        self.total_dispatched.fetch_add(1, Ordering::Relaxed);
        #[cfg(feature = "metrics")]
        counter!("dht_metadata_jobs_dispatched_total").increment(1);

        let job = MetadataJob {
            info_hash: entry.info_hash,
            peers: entry.peers,
            peer_rx,
        };
        let fetcher = self.fetcher.clone();
        let callback = self.callback.clone();
        let on_metadata_fetch = self.on_metadata_fetch.clone();
        let runtime_stats = self.runtime_stats.clone();
        let peer_lookup_tx = self.peer_lookup_tx.clone();
        tasks.spawn(async move {
            Self::process_hash(
                job,
                &fetcher,
                &callback,
                &on_metadata_fetch,
                &runtime_stats,
                peer_lookup_tx,
            )
            .await
        });
    }

    fn handle_job_result(
        &self,
        result: JobResult,
        queue: &mut PendingHashQueue,
        in_flight: &mut HashMap<String, InFlightState>,
    ) {
        if !in_flight.contains_key(&result.info_hash) {
            return;
        }

        match result.outcome {
            JobOutcome::GateRejected => {
                queue.remove(&result.info_hash);
                in_flight.remove(&result.info_hash);
                #[cfg(feature = "metrics")]
                counter!("dht_metadata_jobs_completed_total", "result" => "gate_rejected")
                    .increment(1);
            }
            JobOutcome::Completed(status) => {
                queue.remove(&result.info_hash);
                in_flight.remove(&result.info_hash);
                self.finish(result.info_hash, status, result.attempts);
            }
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
        peer_lookup_tx: Option<mpsc::Sender<[u8; 20]>>,
    ) -> JobResult {
        if let Some(gate) = on_metadata_fetch.load_full()
            && !gate(job.info_hash.clone()).await
        {
            return JobResult {
                info_hash: job.info_hash,
                attempts: 0,
                outcome: JobOutcome::GateRejected,
            };
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
        let lookup_task = peer_lookup_tx.map(|peer_lookup_tx| {
            tokio::spawn(async move {
                tokio::time::sleep(ACTIVE_PEER_LOOKUP_DELAY).await;
                let _ = peer_lookup_tx.try_send(info_hash_bytes);
            })
        });
        let fetched = race_peer_fetches(
            job.peers,
            &mut job.peer_rx,
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
        if let Some(task) = lookup_task {
            task.abort();
        }
        let attempts = attempts.load(Ordering::Relaxed);

        if let Some((peer, (name, total_size, files, piece_length))) = fetched {
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

async fn race_peer_fetches<F, Fut>(
    peers: Vec<PeerCandidate>,
    peer_rx: &mut mpsc::Receiver<PeerCandidate>,
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
    let mut candidates = 0usize;
    for peer in peers {
        let fetch = fetch.clone();
        tasks.spawn(fetch(peer));
        candidates += 1;
        runtime_stats.metadata_peer_candidate();
        #[cfg(feature = "metrics")]
        counter!("dht_metadata_peer_candidates_total", "source" => "initial").increment(1);
    }

    let mut accepting_live_peers = candidates < MAX_METADATA_PEERS_PER_HASH;
    loop {
        if tasks.is_empty() {
            if !accepting_live_peers {
                return None;
            }
            match tokio::time::timeout(LIVE_PEER_IDLE_GRACE, peer_rx.recv()).await {
                Ok(Some(peer)) => {
                    let fetch = fetch.clone();
                    tasks.spawn(fetch(peer));
                    candidates += 1;
                    runtime_stats.metadata_peer_candidate();
                    #[cfg(feature = "metrics")]
                    counter!("dht_metadata_peer_candidates_total", "source" => "live").increment(1);
                    accepting_live_peers = candidates < MAX_METADATA_PEERS_PER_HASH;
                }
                Ok(None) | Err(_) => return None,
            }
            continue;
        }

        tokio::select! {
            result = tasks.join_next() => {
                let Some(Ok((peer, outcome))) = result else {
                    continue;
                };
                if let MetadataFetchOutcome::Fetched(metadata) = outcome {
                    let canceled = tasks.len();
                    runtime_stats.metadata_peer_canceled(canceled);
                    #[cfg(feature = "metrics")]
                    counter!("dht_metadata_peer_canceled_total").increment(canceled as u64);
                    peer_rx.close();
                    tasks.abort_all();
                    while tasks.join_next().await.is_some() {}
                    return Some((peer, metadata));
                }
            }
            peer = peer_rx.recv(), if accepting_live_peers => {
                match peer {
                    Some(peer) => {
                        let fetch = fetch.clone();
                        tasks.spawn(fetch(peer));
                        candidates += 1;
                        runtime_stats.metadata_peer_candidate();
                        #[cfg(feature = "metrics")]
                        counter!("dht_metadata_peer_candidates_total", "source" => "live")
                            .increment(1);
                        accepting_live_peers = candidates < MAX_METADATA_PEERS_PER_HASH;
                    }
                    None => accepting_live_peers = false,
                }
            }
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
        let callback: Arc<MetadataFetchCallback> =
            Arc::new(Box::new(move |hash| Box::pin(callback(hash))));
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
            peer_addr: SocketAddr::from(([127, 0, 0, 1], port)),
            discovered_at,
        }
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
    fn queue_deduplicates_hash_and_keeps_twelve_newest_unique_peers() {
        let start = Instant::now();
        let mut queue = PendingHashQueue::new(10, Duration::from_secs(60));
        for offset in 0..13 {
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
            event("hash", 1012, start + Duration::from_secs(20)),
            start + Duration::from_secs(20),
        );

        assert_eq!(queue.len(), 1);
        let entry = queue.remove("hash").unwrap();
        let ports: Vec<_> = entry.peers.iter().map(|peer| peer.addr.port()).collect();
        assert_eq!(ports, (1001..=1012).rev().collect::<Vec<_>>());
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
    fn in_flight_hash_routes_at_most_twelve_unique_peers_to_live_race() {
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
        let (peer_tx, mut peer_rx) = mpsc::channel(MAX_METADATA_PEERS_PER_HASH);
        let mut in_flight = HashMap::from([(
            "hash".to_string(),
            InFlightState {
                peer_tx,
                scheduled_peers: HashSet::from([SocketAddr::from(([127, 0, 0, 1], 1000))]),
            },
        )]);
        let mut queue = PendingHashQueue::new(4, HASH_QUEUE_TTL);
        let now = Instant::now();

        for port in 1001..=1013 {
            scheduler.enqueue(&mut queue, &mut in_flight, event("hash", port, now));
        }
        scheduler.enqueue(&mut queue, &mut in_flight, event("hash", 1001, now));

        let mut received = Vec::new();
        while let Ok(peer) = peer_rx.try_recv() {
            received.push(peer.addr.port());
        }
        assert_eq!(received, (1001..=1011).collect::<Vec<_>>());
        assert_eq!(
            in_flight["hash"].scheduled_peers.len(),
            MAX_METADATA_PEERS_PER_HASH
        );
        assert!(queue.is_empty());
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.metadata_live_peer_joins, 11);
        assert_eq!(snapshot.metadata_queue_deduplicated, 14);
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
        let (_peer_tx, mut peer_rx) = mpsc::channel(MAX_METADATA_PEERS_PER_HASH);
        let stats = DhtRuntimeStats::default();

        let result = race_peer_fetches(
            peers,
            &mut peer_rx,
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
        let (peer_tx, mut peer_rx) = mpsc::channel(MAX_METADATA_PEERS_PER_HASH);
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
        assert_eq!(snapshot.metadata_peer_canceled, 1);
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
}
