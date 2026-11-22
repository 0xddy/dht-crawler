mod queue;
mod race;
mod retry;
#[cfg(test)]
mod tests;

use crate::metadata::RbitFetcher;
use crate::peer_lookup::PeerLookupRequest;
use crate::runtime_stats::DhtRuntimeStats;
use crate::server::{HashDiscovered, HashSource};
use crate::types::{MetadataFetchCompletion, MetadataFetchCompletionStatus, MetadataOptions, TorrentInfo};
use arc_swap::ArcSwapOption;
#[cfg(feature = "metrics")]
use metrics::{counter, gauge, histogram};
use queue::{HASH_QUEUE_TTL, InFlightState, PeerCandidate, PendingHashQueue, QueuePushKind, QueuedHash, merge_candidate};
use race::{JobPeerLookup, MAX_LOOKUP_DURATION, race_peer_fetches};
use retry::{MAX_DEFER_RECHECKS, RetryHistory};
use std::collections::HashMap;
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
const DISPATCH_TICK: Duration = Duration::from_millis(25);

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

#[derive(Debug)]
struct MetadataJob {
    info_hash: String,
    peers: Vec<PeerCandidate>,
    peer_rx: mpsc::Receiver<PeerCandidate>,
    source_nodes: Vec<crate::types::NodeTuple>,
    source: HashSource,
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
            // Cooling retries consume bounded storage, but are not runnable metadata pressure.
            self.sync_queue_len(queue.len(), queue.pressure_len(), in_flight.len());
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
        if let Some(record) = record {
            if record.failures >= record.attempt_limit(&self.metadata_options) {
                self.runtime_stats
                    .yield_stats()
                    .inc("metadata_retry_exhausted");
                return;
            }
            if !queue.can_defer(&hash.info_hash) {
                self.runtime_stats
                    .yield_stats()
                    .inc("metadata_retry_evicted");
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
            if let Some(entry) = queue.remove(&info_hash)
                && !queue.defer(entry, record)
            {
                self.runtime_stats
                    .yield_stats()
                    .inc("metadata_retry_evicted");
                return;
            }
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
        if (!failed && record.defers > MAX_DEFER_RECHECKS)
            || record.failures >= record.attempt_limit(&self.metadata_options)
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
            source: job.source,
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

    fn sync_queue_len(&self, queued: usize, pressure_queued: usize, in_flight: usize) {
        let depth = queued.saturating_add(in_flight);
        let pressure_depth = pressure_queued.saturating_add(in_flight);
        self.queue_len.store(pressure_depth, Ordering::Relaxed);
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
