use crate::metadata::{MetadataFetchOutcome, RbitFetcher};
#[cfg(test)]
use crate::runtime_stats::DhtRuntimeLimits;
use crate::runtime_stats::DhtRuntimeStats;
use crate::server::HashDiscovered;
use crate::types::{MetadataFetchCompletion, MetadataFetchCompletionStatus, TorrentInfo};
use arc_swap::ArcSwapOption;
#[cfg(feature = "metrics")]
use metrics::{counter, gauge, histogram};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

const MAX_PEERS_PER_HASH: usize = 3;
const HASH_QUEUE_TTL: Duration = Duration::from_secs(60);

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

#[derive(Debug, Clone, Copy)]
struct PeerCandidate {
    addr: SocketAddr,
    discovered_at: Instant,
}

#[derive(Debug)]
struct QueuedHash {
    info_hash: String,
    peers: Vec<PeerCandidate>,
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
            entry.peers.truncate(MAX_PEERS_PER_HASH);
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

    fn pop_newest_available(
        &mut self,
        in_flight: &HashMap<String, InFlightState>,
    ) -> Option<QueuedHash> {
        let info_hash = self
            .order
            .iter()
            .rev()
            .find_map(|(_, hash)| (!in_flight.contains_key(hash)).then(|| hash.clone()))?;
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
    already_admitted: bool,
}

#[derive(Debug)]
struct InFlightState {
    attempts: usize,
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
            runtime_stats,
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
        let mut stats_interval = tokio::time::interval(Duration::from_secs(60));
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
                        Some(hash) => self.enqueue(&mut queue, hash),
                        None => input_closed = true,
                    }
                }
                result = tasks.join_next(), if !tasks.is_empty() => {
                    match result {
                        Some(Ok(result)) => {
                            self.handle_job_result(result, &mut queue, &mut in_flight, &mut tasks);
                        }
                        Some(Err(error)) => {
                            #[cfg(feature = "metrics")]
                            counter!("dht_metadata_worker_join_error_total").increment(1);
                            log::warn!("Metadata worker task failed: {error}");
                        }
                        None => {}
                    }
                }
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

    fn enqueue(&self, queue: &mut PendingHashQueue, hash: HashDiscovered) {
        self.total_received.fetch_add(1, Ordering::Relaxed);
        let kind = queue.push(hash, Instant::now());
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
            let Some(entry) = queue.pop_newest_available(in_flight) else {
                break;
            };
            let queue_wait = Instant::now()
                .checked_duration_since(entry.latest_at)
                .unwrap_or_default();
            self.runtime_stats.observe_metadata_queue_wait(
                queue_wait.as_millis().min(u128::from(u64::MAX)) as u64,
            );
            #[cfg(feature = "metrics")]
            histogram!("dht_metadata_queue_wait_seconds").record(queue_wait.as_secs_f64());
            let state = InFlightState {
                attempts: 0,
                scheduled_peers: HashSet::new(),
            };
            in_flight.insert(entry.info_hash.clone(), state);
            self.spawn_job(entry, false, in_flight, tasks);
        }
    }

    fn spawn_job(
        &self,
        mut entry: QueuedHash,
        already_admitted: bool,
        in_flight: &mut HashMap<String, InFlightState>,
        tasks: &mut JoinSet<JobResult>,
    ) {
        let Some(state) = in_flight.get_mut(&entry.info_hash) else {
            return;
        };
        let remaining = MAX_PEERS_PER_HASH.saturating_sub(state.attempts);
        let now = Instant::now();
        entry.peers.retain(|peer| {
            !state.scheduled_peers.contains(&peer.addr)
                && now
                    .checked_duration_since(peer.discovered_at)
                    .unwrap_or_default()
                    <= HASH_QUEUE_TTL
        });
        entry.peers.truncate(remaining);
        for peer in &entry.peers {
            state.scheduled_peers.insert(peer.addr);
        }

        if entry.peers.is_empty() {
            let attempts = state.attempts;
            if already_admitted {
                self.finish(
                    entry.info_hash.clone(),
                    MetadataFetchCompletionStatus::FetchFailed,
                    attempts,
                );
            } else {
                self.total_dropped.fetch_add(1, Ordering::Relaxed);
                self.runtime_stats.metadata_queue_stale(1);
                #[cfg(feature = "metrics")]
                counter!("dht_metadata_queue_events_total", "result" => "expired").increment(1);
            }
            in_flight.remove(&entry.info_hash);
            return;
        }

        self.total_dispatched.fetch_add(1, Ordering::Relaxed);
        #[cfg(feature = "metrics")]
        counter!("dht_metadata_jobs_dispatched_total").increment(1);

        let job = MetadataJob {
            info_hash: entry.info_hash,
            peers: entry.peers,
            already_admitted,
        };
        let fetcher = self.fetcher.clone();
        let callback = self.callback.clone();
        let on_metadata_fetch = self.on_metadata_fetch.clone();
        tasks.spawn(async move {
            Self::process_hash(job, &fetcher, &callback, &on_metadata_fetch).await
        });
    }

    fn handle_job_result(
        &self,
        result: JobResult,
        queue: &mut PendingHashQueue,
        in_flight: &mut HashMap<String, InFlightState>,
        tasks: &mut JoinSet<JobResult>,
    ) {
        let Some(state) = in_flight.get_mut(&result.info_hash) else {
            return;
        };
        state.attempts = state.attempts.saturating_add(result.attempts);

        match result.outcome {
            JobOutcome::GateRejected => {
                queue.remove(&result.info_hash);
                in_flight.remove(&result.info_hash);
                #[cfg(feature = "metrics")]
                counter!("dht_metadata_jobs_completed_total", "result" => "gate_rejected")
                    .increment(1);
            }
            JobOutcome::Completed(MetadataFetchCompletionStatus::FetchFailed)
                if state.attempts < MAX_PEERS_PER_HASH =>
            {
                if let Some(entry) = queue.remove(&result.info_hash) {
                    self.spawn_job(entry, true, in_flight, tasks);
                } else {
                    let attempts = state.attempts;
                    in_flight.remove(&result.info_hash);
                    self.finish(
                        result.info_hash,
                        MetadataFetchCompletionStatus::FetchFailed,
                        attempts,
                    );
                }
            }
            JobOutcome::Completed(status) => {
                let attempts = state.attempts;
                queue.remove(&result.info_hash);
                in_flight.remove(&result.info_hash);
                self.finish(result.info_hash, status, attempts);
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
        job: MetadataJob,
        fetcher: &Arc<RbitFetcher>,
        callback: &Arc<ArcSwapOption<TorrentAckCallback>>,
        on_metadata_fetch: &Arc<ArcSwapOption<MetadataFetchCallback>>,
    ) -> JobResult {
        if !job.already_admitted
            && let Some(gate) = on_metadata_fetch.load_full()
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

        let mut attempts = 0;
        for peer in job.peers {
            let (name, total_size, files, piece_length) =
                match fetcher.fetch(&info_hash_bytes, peer.addr).await {
                    MetadataFetchOutcome::Fetched(metadata) => {
                        attempts += 1;
                        metadata
                    }
                    MetadataFetchOutcome::Failed => {
                        attempts += 1;
                        continue;
                    }
                    MetadataFetchOutcome::SkippedCached => continue,
                };

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

#[cfg(test)]
mod tests {
    use super::*;

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

        scheduler.enqueue(&mut queue, event("old", 1000, start));
        scheduler.enqueue(
            &mut queue,
            event("old", 1001, start + Duration::from_secs(1)),
        );
        scheduler.enqueue(
            &mut queue,
            event("middle", 1002, start + Duration::from_secs(2)),
        );
        scheduler.enqueue(
            &mut queue,
            event("new", 1003, start + Duration::from_secs(3)),
        );
        scheduler.enqueue(
            &mut queue,
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
    fn queue_deduplicates_hash_and_keeps_newest_unique_peers() {
        let start = Instant::now();
        let mut queue = PendingHashQueue::new(10, Duration::from_secs(60));
        for offset in 0..4 {
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
            event("hash", 1003, start + Duration::from_secs(5)),
            start + Duration::from_secs(5),
        );

        assert_eq!(queue.len(), 1);
        let entry = queue.remove("hash").unwrap();
        let ports: Vec<_> = entry.peers.iter().map(|peer| peer.addr.port()).collect();
        assert_eq!(ports, vec![1003, 1002, 1001]);
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
        let newest = queue.pop_newest_available(&in_flight).unwrap();
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
