use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

pub const METADATA_QUEUE_WAIT_BUCKETS_MS: [u64; 8] = [10, 50, 100, 250, 500, 1_000, 2_000, 5_000];
pub const METADATA_FETCH_BUCKETS_MS: [u64; 7] = [250, 500, 1_000, 2_000, 4_000, 6_000, 10_000];
pub const METADATA_SIZE_BUCKETS_BYTES: [u64; 8] = [
    16 * 1024,
    32 * 1024,
    64 * 1024,
    128 * 1024,
    256 * 1024,
    512 * 1024,
    1024 * 1024,
    10 * 1024 * 1024,
];

#[derive(Debug, Clone, PartialEq, Eq)]
/// Snapshot of a non-cumulative fixed-bucket histogram.
pub struct FixedHistogramSnapshot {
    /// Inclusive upper bound for each bucket.
    pub bounds: Vec<u64>,
    /// Per-bucket counts; values are not cumulative.
    pub counts: Vec<u64>,
    /// Values larger than the final bound.
    pub overflow: u64,
    /// Total observations including overflow.
    pub count: u64,
    /// Wrapping sum of all observed values.
    pub sum: u64,
}

impl FixedHistogramSnapshot {
    /// Returns the inclusive bucket bound containing the requested approximate percentile.
    ///
    /// `percentile` is clamped to `0.0..=1.0`. Overflow observations return the final finite
    /// bound because the histogram intentionally stores no dynamic maximum.
    pub fn percentile(&self, percentile: f64) -> Option<u64> {
        if self.count == 0 {
            return None;
        }
        let rank = ((self.count as f64 * percentile.clamp(0.0, 1.0)).ceil() as u64).max(1);
        let mut cumulative = 0u64;
        for (bound, count) in self.bounds.iter().zip(&self.counts) {
            cumulative = cumulative.saturating_add(*count);
            if cumulative >= rank {
                return Some(*bound);
            }
        }
        self.bounds.last().copied()
    }
}

struct AtomicFixedHistogram<const N: usize> {
    bounds: [u64; N],
    counts: [AtomicU64; N],
    overflow: AtomicU64,
    count: AtomicU64,
    sum: AtomicU64,
}

impl<const N: usize> AtomicFixedHistogram<N> {
    fn new(bounds: [u64; N]) -> Self {
        Self {
            bounds,
            counts: std::array::from_fn(|_| AtomicU64::new(0)),
            overflow: AtomicU64::new(0),
            count: AtomicU64::new(0),
            sum: AtomicU64::new(0),
        }
    }

    fn record(&self, value: u64) {
        if let Some(index) = self.bounds.iter().position(|bound| value <= *bound) {
            self.counts[index].fetch_add(1, Ordering::Relaxed);
        } else {
            self.overflow.fetch_add(1, Ordering::Relaxed);
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum.fetch_add(value, Ordering::Relaxed);
    }

    fn snapshot(&self) -> FixedHistogramSnapshot {
        FixedHistogramSnapshot {
            bounds: self.bounds.to_vec(),
            counts: self
                .counts
                .iter()
                .map(|count| count.load(Ordering::Relaxed))
                .collect(),
            overflow: self.overflow.load(Ordering::Relaxed),
            count: self.count.load(Ordering::Relaxed),
            sum: self.sum.load(Ordering::Relaxed),
        }
    }
}

impl<const N: usize> Default for AtomicFixedHistogram<N> {
    fn default() -> Self {
        Self::new([0; N])
    }
}

/// Transport-neutral counters and fixed-bucket histograms used by dashboards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DhtObservabilitySnapshot {
    /// UDP datagrams received before validation.
    pub udp_rx_packets: u64,
    /// UDP bytes received before validation.
    pub udp_rx_bytes: u64,
    /// Successfully sent UDP query and response datagrams.
    pub udp_tx_packets: u64,
    /// Successfully sent UDP query and response bytes.
    pub udp_tx_bytes: u64,
    /// Inbound ping queries.
    pub inbound_ping: u64,
    /// Inbound find_node queries.
    pub inbound_find_node: u64,
    /// Inbound get_peers queries.
    pub inbound_get_peers: u64,
    /// Inbound announce_peer queries.
    pub inbound_announce_peer: u64,
    /// Other or invalid inbound query names.
    pub inbound_other: u64,
    /// Replies admitted by the regular response budget.
    pub response_normal: u64,
    /// Priority ping/get_peers replies admitted by the reserve.
    pub response_fallback: u64,
    /// Replies rejected by final response limits.
    pub response_rate_limited: u64,
    /// Admitted replies whose `send_to` failed.
    pub response_send_failed: u64,
    /// Valid, unfiltered announces with a usable Peer port.
    pub announce_accepted: u64,
    /// Announces with a missing or invalid token.
    pub announce_invalid_token: u64,
    /// Announces rejected by the application Hash filter.
    pub announce_filtered: u64,
    /// New nodes admitted to the FIFO pool.
    pub node_admitted: u64,
    /// Full-pool replacements.
    pub node_replaced: u64,
    /// Nodes rejected as queued or recently probed duplicates.
    pub node_dropped_duplicate: u64,
    /// Nodes rejected by the replacement budget.
    pub node_dropped_rate_limited: u64,
    /// Nodes rejected because their endpoint is not usable.
    pub node_dropped_invalid: u64,
    /// BEP-9 Metadata piece payload bytes received.
    pub metadata_bytes_downloaded: u64,
    /// Metadata Peer attempts reaching the end-to-end timeout.
    pub metadata_failure_timeout: u64,
    /// Metadata Peer connection failures.
    pub metadata_failure_connect: u64,
    /// Peers without extension-protocol support.
    pub metadata_failure_no_extension: u64,
    /// Extension handshake or piece-request send failures.
    pub metadata_failure_send: u64,
    /// Metadata payloads exceeding the 10 MiB limit.
    pub metadata_failure_size_limit: u64,
    /// Metadata payloads failing InfoHash SHA1 validation.
    pub metadata_failure_sha1: u64,
    /// Validated payloads that could not be parsed as torrent info.
    pub metadata_failure_parse: u64,
    /// Other receive, piece or worker failures.
    pub metadata_failure_other: u64,
    /// Failure-cache hits for previously timed-out Peers.
    pub peer_cache_timeout_hits: u64,
    /// Failure-cache hits for previous connection failures.
    pub peer_cache_connect_hits: u64,
    /// Queue wait observations in milliseconds.
    pub queue_wait_ms: FixedHistogramSnapshot,
    /// End-to-end Peer attempt observations in milliseconds.
    pub fetch_duration_ms: FixedHistogramSnapshot,
    /// Complete bencoded info payload observations in bytes.
    pub metadata_size_bytes: FixedHistogramSnapshot,
}

/// Cheap, cloneable handle for reading live DHT runtime statistics.
#[derive(Clone, Default)]
pub struct DhtRuntimeStats {
    inner: Arc<DhtRuntimeStatsInner>,
}

/// Point-in-time view returned by [`DhtRuntimeStats::snapshot`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DhtRuntimeSnapshot {
    /// Valid announce hashes observed before ingress publication.
    pub hashes_received: u64,
    /// Hashes dropped because bounded ingress was full.
    pub hash_ingress_dropped: u64,
    /// Current hash-ingress queue depth.
    pub hash_ingress_queue_depth: usize,
    /// Configured hash-ingress queue capacity.
    pub hash_ingress_queue_capacity: usize,

    /// Current priority crawl-event depth.
    pub crawl_priority_queue_depth: usize,
    /// Configured priority crawl-event capacity.
    pub crawl_priority_queue_capacity: usize,
    /// Current discovered-node event depth.
    pub crawl_discovery_queue_depth: usize,
    /// Configured discovered-node event capacity.
    pub crawl_discovery_queue_capacity: usize,

    /// Deduplicated pending Metadata Hashes.
    pub metadata_queue_depth: usize,
    /// Configured pending Metadata capacity.
    pub metadata_queue_max: usize,
    /// Current Metadata jobs.
    pub metadata_in_flight: usize,
    /// Unique Hash insertions, including insertions that evicted an older Hash.
    pub metadata_queue_inserted: u64,
    /// Duplicate Hash updates merged into existing entries.
    pub metadata_queue_deduplicated: u64,
    /// Oldest Hashes evicted by newer events.
    pub metadata_queue_evicted: u64,
    /// Events rejected or expired because they were stale.
    pub metadata_queue_stale: u64,
    /// Metadata Peer races started after admission.
    pub metadata_races_started: u64,
    /// Distinct Peer candidates launched into Metadata races.
    pub metadata_peer_candidates: u64,
    /// Peer candidates added while a Metadata race was already running.
    pub metadata_live_peer_joins: u64,
    /// Losing Peer attempts canceled after another Peer succeeded.
    pub metadata_peer_canceled: u64,
    /// Active `get_peers` lookup requests received from Metadata jobs.
    pub peer_lookup_requested: u64,
    /// Active `get_peers` lookups admitted by the bounded rate limiter.
    pub peer_lookup_started: u64,
    /// Active lookup requests rejected by concurrency or rate limits.
    pub peer_lookup_rate_limited: u64,
    /// Active lookup requests skipped because the routing snapshot was empty.
    pub peer_lookup_empty: u64,
    /// Active `get_peers` UDP queries sent.
    pub peer_lookup_queries: u64,
    /// Matched active `get_peers` responses.
    pub peer_lookup_responses: u64,
    /// Active `get_peers` queries that timed out.
    pub peer_lookup_timeouts: u64,
    /// Active `get_peers` UDP sends that failed immediately.
    pub peer_lookup_send_failures: u64,
    /// Unique Peer endpoints fed back into Metadata scheduling.
    pub peer_lookup_peers_found: u64,
    /// Tagged lookup responses dropped before reaching the lookup actor.
    pub peer_lookup_response_dropped: u64,
    /// Discovered Peer endpoints dropped because Hash ingress was full.
    pub peer_lookup_output_dropped: u64,
    /// Real Peer network attempts.
    pub metadata_peer_attempts: u64,
    /// Successful Peer downloads and parses.
    pub metadata_peer_succeeded: u64,
    /// Failed real Peer attempts.
    pub metadata_peer_failed: u64,
    /// End-to-end Peer timeouts.
    pub metadata_peer_timeouts: u64,
    /// Peer connection failures.
    pub metadata_connect_failed: u64,
    /// Peers without extension-protocol support.
    pub metadata_no_extension: u64,
    /// Peer failure-cache skips.
    pub metadata_peer_failure_cache_hits: u64,
    /// Current Peer failure-cache entries.
    pub metadata_peer_failure_cache_entries: usize,

    /// Current strict FIFO crawl-pool size.
    pub node_pool_size: usize,
    /// Configured FIFO capacity.
    pub node_pool_capacity: usize,
    /// Bootstrap low-water mark.
    pub node_pool_low_watermark: usize,

    /// Current pending find_node transactions.
    pub find_node_in_flight: usize,
    /// Configured total find_node in-flight limit.
    pub find_node_in_flight_max: usize,
    /// Metadata-pressure-adjusted find_node budget per second.
    pub find_node_effective_rate_per_sec: u32,
    /// Queries sent to never-before-probed destinations.
    pub queries_new: u64,
    /// Queries sent to responsive revisit nodes.
    pub queries_revisit: u64,
    /// Queries sent to bootstrap endpoints.
    pub queries_bootstrap: u64,
    /// Replies matched to pending transactions.
    pub responses: u64,
    /// Replies without a matching pending transaction.
    pub unmatched_responses: u64,
    /// Pending transactions that expired.
    pub timeouts: u64,
    /// Outbound find_node send failures.
    pub send_failures: u64,
    /// Response events dropped before reaching the actor.
    pub crawl_events_dropped_response: u64,
    /// Discovered-node events dropped before reaching the actor.
    pub crawl_events_dropped_discovered: u64,

    /// UDP datagrams received before validation.
    pub udp_received: u64,
    /// Valid datagrams dropped because every worker queue was full.
    pub udp_queue_full: u64,
    /// Empty, oversized or non-bencoded UDP datagrams.
    pub udp_invalid: u64,
    /// DHT replies rejected by final rate limits.
    pub udp_responses_rate_limited: u64,
    /// Priority replies admitted by the ping/get_peers reserve.
    pub udp_responses_priority_reserved: u64,
}

struct DhtRuntimeStatsInner {
    hashes_received: AtomicU64,
    hash_ingress_dropped: AtomicU64,
    hash_ingress_queue_depth: AtomicUsize,
    hash_ingress_queue_capacity: usize,

    crawl_priority_queue_depth: AtomicUsize,
    crawl_priority_queue_capacity: usize,
    crawl_discovery_queue_depth: AtomicUsize,
    crawl_discovery_queue_capacity: usize,

    metadata_queue_depth: AtomicUsize,
    metadata_queue_max: usize,
    metadata_in_flight: AtomicUsize,
    metadata_queue_inserted: AtomicU64,
    metadata_queue_deduplicated: AtomicU64,
    metadata_queue_evicted: AtomicU64,
    metadata_queue_stale: AtomicU64,
    metadata_races_started: AtomicU64,
    metadata_peer_candidates: AtomicU64,
    metadata_live_peer_joins: AtomicU64,
    metadata_peer_canceled: AtomicU64,
    peer_lookup_requested: AtomicU64,
    peer_lookup_started: AtomicU64,
    peer_lookup_rate_limited: AtomicU64,
    peer_lookup_empty: AtomicU64,
    peer_lookup_queries: AtomicU64,
    peer_lookup_responses: AtomicU64,
    peer_lookup_timeouts: AtomicU64,
    peer_lookup_send_failures: AtomicU64,
    peer_lookup_peers_found: AtomicU64,
    peer_lookup_response_dropped: AtomicU64,
    peer_lookup_output_dropped: AtomicU64,
    metadata_peer_attempts: AtomicU64,
    metadata_peer_succeeded: AtomicU64,
    metadata_peer_failed: AtomicU64,
    metadata_peer_timeouts: AtomicU64,
    metadata_connect_failed: AtomicU64,
    metadata_no_extension: AtomicU64,
    metadata_peer_failure_cache_hits: AtomicU64,
    metadata_peer_failure_cache_entries: AtomicUsize,

    node_pool_size: AtomicUsize,
    node_pool_capacity: usize,
    node_pool_low_watermark: usize,

    find_node_in_flight: AtomicUsize,
    find_node_in_flight_max: usize,
    find_node_effective_rate_per_sec: AtomicU32,
    queries_new: AtomicU64,
    queries_revisit: AtomicU64,
    queries_bootstrap: AtomicU64,
    responses: AtomicU64,
    unmatched_responses: AtomicU64,
    timeouts: AtomicU64,
    send_failures: AtomicU64,
    crawl_events_dropped_response: AtomicU64,
    crawl_events_dropped_discovered: AtomicU64,

    udp_received: AtomicU64,
    udp_queue_full: AtomicU64,
    udp_invalid: AtomicU64,
    udp_responses_rate_limited: AtomicU64,
    udp_responses_priority_reserved: AtomicU64,

    udp_rx_bytes: AtomicU64,
    udp_tx_packets: AtomicU64,
    udp_tx_bytes: AtomicU64,
    inbound_ping: AtomicU64,
    inbound_find_node: AtomicU64,
    inbound_get_peers: AtomicU64,
    inbound_announce_peer: AtomicU64,
    inbound_other: AtomicU64,
    response_normal: AtomicU64,
    response_send_failed: AtomicU64,
    announce_accepted: AtomicU64,
    announce_invalid_token: AtomicU64,
    announce_filtered: AtomicU64,
    node_admitted: AtomicU64,
    node_replaced: AtomicU64,
    node_dropped_duplicate: AtomicU64,
    node_dropped_rate_limited: AtomicU64,
    node_dropped_invalid: AtomicU64,
    metadata_bytes_downloaded: AtomicU64,
    metadata_failure_timeout: AtomicU64,
    metadata_failure_connect: AtomicU64,
    metadata_failure_no_extension: AtomicU64,
    metadata_failure_send: AtomicU64,
    metadata_failure_size_limit: AtomicU64,
    metadata_failure_sha1: AtomicU64,
    metadata_failure_parse: AtomicU64,
    metadata_failure_other: AtomicU64,
    peer_cache_timeout_hits: AtomicU64,
    peer_cache_connect_hits: AtomicU64,
    queue_wait_ms: AtomicFixedHistogram<8>,
    fetch_duration_ms: AtomicFixedHistogram<7>,
    metadata_size_bytes: AtomicFixedHistogram<8>,
}

impl Default for DhtRuntimeStatsInner {
    fn default() -> Self {
        Self {
            queue_wait_ms: AtomicFixedHistogram::new(METADATA_QUEUE_WAIT_BUCKETS_MS),
            fetch_duration_ms: AtomicFixedHistogram::new(METADATA_FETCH_BUCKETS_MS),
            metadata_size_bytes: AtomicFixedHistogram::new(METADATA_SIZE_BUCKETS_BYTES),
            hashes_received: AtomicU64::new(0),
            hash_ingress_dropped: AtomicU64::new(0),
            hash_ingress_queue_depth: AtomicUsize::new(0),
            hash_ingress_queue_capacity: 0,
            crawl_priority_queue_depth: AtomicUsize::new(0),
            crawl_priority_queue_capacity: 0,
            crawl_discovery_queue_depth: AtomicUsize::new(0),
            crawl_discovery_queue_capacity: 0,
            metadata_queue_depth: AtomicUsize::new(0),
            metadata_queue_max: 0,
            metadata_in_flight: AtomicUsize::new(0),
            metadata_queue_inserted: AtomicU64::new(0),
            metadata_queue_deduplicated: AtomicU64::new(0),
            metadata_queue_evicted: AtomicU64::new(0),
            metadata_queue_stale: AtomicU64::new(0),
            metadata_races_started: AtomicU64::new(0),
            metadata_peer_candidates: AtomicU64::new(0),
            metadata_live_peer_joins: AtomicU64::new(0),
            metadata_peer_canceled: AtomicU64::new(0),
            peer_lookup_requested: AtomicU64::new(0),
            peer_lookup_started: AtomicU64::new(0),
            peer_lookup_rate_limited: AtomicU64::new(0),
            peer_lookup_empty: AtomicU64::new(0),
            peer_lookup_queries: AtomicU64::new(0),
            peer_lookup_responses: AtomicU64::new(0),
            peer_lookup_timeouts: AtomicU64::new(0),
            peer_lookup_send_failures: AtomicU64::new(0),
            peer_lookup_peers_found: AtomicU64::new(0),
            peer_lookup_response_dropped: AtomicU64::new(0),
            peer_lookup_output_dropped: AtomicU64::new(0),
            metadata_peer_attempts: AtomicU64::new(0),
            metadata_peer_succeeded: AtomicU64::new(0),
            metadata_peer_failed: AtomicU64::new(0),
            metadata_peer_timeouts: AtomicU64::new(0),
            metadata_connect_failed: AtomicU64::new(0),
            metadata_no_extension: AtomicU64::new(0),
            metadata_peer_failure_cache_hits: AtomicU64::new(0),
            metadata_peer_failure_cache_entries: AtomicUsize::new(0),
            node_pool_size: AtomicUsize::new(0),
            node_pool_capacity: 0,
            node_pool_low_watermark: 0,
            find_node_in_flight: AtomicUsize::new(0),
            find_node_in_flight_max: 0,
            find_node_effective_rate_per_sec: AtomicU32::new(0),
            queries_new: AtomicU64::new(0),
            queries_revisit: AtomicU64::new(0),
            queries_bootstrap: AtomicU64::new(0),
            responses: AtomicU64::new(0),
            unmatched_responses: AtomicU64::new(0),
            timeouts: AtomicU64::new(0),
            send_failures: AtomicU64::new(0),
            crawl_events_dropped_response: AtomicU64::new(0),
            crawl_events_dropped_discovered: AtomicU64::new(0),
            udp_received: AtomicU64::new(0),
            udp_queue_full: AtomicU64::new(0),
            udp_invalid: AtomicU64::new(0),
            udp_responses_rate_limited: AtomicU64::new(0),
            udp_responses_priority_reserved: AtomicU64::new(0),
            udp_rx_bytes: AtomicU64::new(0),
            udp_tx_packets: AtomicU64::new(0),
            udp_tx_bytes: AtomicU64::new(0),
            inbound_ping: AtomicU64::new(0),
            inbound_find_node: AtomicU64::new(0),
            inbound_get_peers: AtomicU64::new(0),
            inbound_announce_peer: AtomicU64::new(0),
            inbound_other: AtomicU64::new(0),
            response_normal: AtomicU64::new(0),
            response_send_failed: AtomicU64::new(0),
            announce_accepted: AtomicU64::new(0),
            announce_invalid_token: AtomicU64::new(0),
            announce_filtered: AtomicU64::new(0),
            node_admitted: AtomicU64::new(0),
            node_replaced: AtomicU64::new(0),
            node_dropped_duplicate: AtomicU64::new(0),
            node_dropped_rate_limited: AtomicU64::new(0),
            node_dropped_invalid: AtomicU64::new(0),
            metadata_bytes_downloaded: AtomicU64::new(0),
            metadata_failure_timeout: AtomicU64::new(0),
            metadata_failure_connect: AtomicU64::new(0),
            metadata_failure_no_extension: AtomicU64::new(0),
            metadata_failure_send: AtomicU64::new(0),
            metadata_failure_size_limit: AtomicU64::new(0),
            metadata_failure_sha1: AtomicU64::new(0),
            metadata_failure_parse: AtomicU64::new(0),
            metadata_failure_other: AtomicU64::new(0),
            peer_cache_timeout_hits: AtomicU64::new(0),
            peer_cache_connect_hits: AtomicU64::new(0),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DhtRuntimeLimits {
    pub metadata_queue: usize,
    pub node_pool: usize,
    pub node_pool_low_watermark: usize,
    pub find_node_in_flight: usize,
    pub initial_find_node_rate: u32,
    pub hash_ingress_queue: usize,
    pub crawl_priority_queue: usize,
    pub crawl_discovery_queue: usize,
}

impl DhtRuntimeStats {
    pub(crate) fn with_limits(limits: DhtRuntimeLimits) -> Self {
        Self {
            inner: Arc::new(DhtRuntimeStatsInner {
                metadata_queue_max: limits.metadata_queue,
                node_pool_capacity: limits.node_pool,
                node_pool_low_watermark: limits.node_pool_low_watermark,
                find_node_in_flight_max: limits.find_node_in_flight,
                find_node_effective_rate_per_sec: AtomicU32::new(limits.initial_find_node_rate),
                hash_ingress_queue_capacity: limits.hash_ingress_queue,
                crawl_priority_queue_capacity: limits.crawl_priority_queue,
                crawl_discovery_queue_capacity: limits.crawl_discovery_queue,
                ..DhtRuntimeStatsInner::default()
            }),
        }
    }

    /// Load every exposed value once using relaxed atomics.
    /// Loads the core runtime counters and gauges using relaxed atomics.
    pub fn snapshot(&self) -> DhtRuntimeSnapshot {
        let inner = &self.inner;
        DhtRuntimeSnapshot {
            hashes_received: inner.hashes_received.load(Ordering::Relaxed),
            hash_ingress_dropped: inner.hash_ingress_dropped.load(Ordering::Relaxed),
            hash_ingress_queue_depth: inner.hash_ingress_queue_depth.load(Ordering::Relaxed),
            hash_ingress_queue_capacity: inner.hash_ingress_queue_capacity,
            crawl_priority_queue_depth: inner.crawl_priority_queue_depth.load(Ordering::Relaxed),
            crawl_priority_queue_capacity: inner.crawl_priority_queue_capacity,
            crawl_discovery_queue_depth: inner.crawl_discovery_queue_depth.load(Ordering::Relaxed),
            crawl_discovery_queue_capacity: inner.crawl_discovery_queue_capacity,
            metadata_queue_depth: inner.metadata_queue_depth.load(Ordering::Relaxed),
            metadata_queue_max: inner.metadata_queue_max,
            metadata_in_flight: inner.metadata_in_flight.load(Ordering::Relaxed),
            metadata_queue_inserted: inner.metadata_queue_inserted.load(Ordering::Relaxed),
            metadata_queue_deduplicated: inner.metadata_queue_deduplicated.load(Ordering::Relaxed),
            metadata_queue_evicted: inner.metadata_queue_evicted.load(Ordering::Relaxed),
            metadata_queue_stale: inner.metadata_queue_stale.load(Ordering::Relaxed),
            metadata_races_started: inner.metadata_races_started.load(Ordering::Relaxed),
            metadata_peer_candidates: inner.metadata_peer_candidates.load(Ordering::Relaxed),
            metadata_live_peer_joins: inner.metadata_live_peer_joins.load(Ordering::Relaxed),
            metadata_peer_canceled: inner.metadata_peer_canceled.load(Ordering::Relaxed),
            peer_lookup_requested: inner.peer_lookup_requested.load(Ordering::Relaxed),
            peer_lookup_started: inner.peer_lookup_started.load(Ordering::Relaxed),
            peer_lookup_rate_limited: inner.peer_lookup_rate_limited.load(Ordering::Relaxed),
            peer_lookup_empty: inner.peer_lookup_empty.load(Ordering::Relaxed),
            peer_lookup_queries: inner.peer_lookup_queries.load(Ordering::Relaxed),
            peer_lookup_responses: inner.peer_lookup_responses.load(Ordering::Relaxed),
            peer_lookup_timeouts: inner.peer_lookup_timeouts.load(Ordering::Relaxed),
            peer_lookup_send_failures: inner.peer_lookup_send_failures.load(Ordering::Relaxed),
            peer_lookup_peers_found: inner.peer_lookup_peers_found.load(Ordering::Relaxed),
            peer_lookup_response_dropped: inner
                .peer_lookup_response_dropped
                .load(Ordering::Relaxed),
            peer_lookup_output_dropped: inner.peer_lookup_output_dropped.load(Ordering::Relaxed),
            metadata_peer_attempts: inner.metadata_peer_attempts.load(Ordering::Relaxed),
            metadata_peer_succeeded: inner.metadata_peer_succeeded.load(Ordering::Relaxed),
            metadata_peer_failed: inner.metadata_peer_failed.load(Ordering::Relaxed),
            metadata_peer_timeouts: inner.metadata_peer_timeouts.load(Ordering::Relaxed),
            metadata_connect_failed: inner.metadata_connect_failed.load(Ordering::Relaxed),
            metadata_no_extension: inner.metadata_no_extension.load(Ordering::Relaxed),
            metadata_peer_failure_cache_hits: inner
                .metadata_peer_failure_cache_hits
                .load(Ordering::Relaxed),
            metadata_peer_failure_cache_entries: inner
                .metadata_peer_failure_cache_entries
                .load(Ordering::Relaxed),
            node_pool_size: inner.node_pool_size.load(Ordering::Relaxed),
            node_pool_capacity: inner.node_pool_capacity,
            node_pool_low_watermark: inner.node_pool_low_watermark,
            find_node_in_flight: inner.find_node_in_flight.load(Ordering::Relaxed),
            find_node_in_flight_max: inner.find_node_in_flight_max,
            find_node_effective_rate_per_sec: inner
                .find_node_effective_rate_per_sec
                .load(Ordering::Relaxed),
            queries_new: inner.queries_new.load(Ordering::Relaxed),
            queries_revisit: inner.queries_revisit.load(Ordering::Relaxed),
            queries_bootstrap: inner.queries_bootstrap.load(Ordering::Relaxed),
            responses: inner.responses.load(Ordering::Relaxed),
            unmatched_responses: inner.unmatched_responses.load(Ordering::Relaxed),
            timeouts: inner.timeouts.load(Ordering::Relaxed),
            send_failures: inner.send_failures.load(Ordering::Relaxed),
            crawl_events_dropped_response: inner
                .crawl_events_dropped_response
                .load(Ordering::Relaxed),
            crawl_events_dropped_discovered: inner
                .crawl_events_dropped_discovered
                .load(Ordering::Relaxed),
            udp_received: inner.udp_received.load(Ordering::Relaxed),
            udp_queue_full: inner.udp_queue_full.load(Ordering::Relaxed),
            udp_invalid: inner.udp_invalid.load(Ordering::Relaxed),
            udp_responses_rate_limited: inner.udp_responses_rate_limited.load(Ordering::Relaxed),
            udp_responses_priority_reserved: inner
                .udp_responses_priority_reserved
                .load(Ordering::Relaxed),
        }
    }

    /// Loads detailed transport counters, failure categories and fixed histograms.
    pub fn observability_snapshot(&self) -> DhtObservabilitySnapshot {
        let inner = &self.inner;
        DhtObservabilitySnapshot {
            udp_rx_packets: inner.udp_received.load(Ordering::Relaxed),
            udp_rx_bytes: inner.udp_rx_bytes.load(Ordering::Relaxed),
            udp_tx_packets: inner.udp_tx_packets.load(Ordering::Relaxed),
            udp_tx_bytes: inner.udp_tx_bytes.load(Ordering::Relaxed),
            inbound_ping: inner.inbound_ping.load(Ordering::Relaxed),
            inbound_find_node: inner.inbound_find_node.load(Ordering::Relaxed),
            inbound_get_peers: inner.inbound_get_peers.load(Ordering::Relaxed),
            inbound_announce_peer: inner.inbound_announce_peer.load(Ordering::Relaxed),
            inbound_other: inner.inbound_other.load(Ordering::Relaxed),
            response_normal: inner.response_normal.load(Ordering::Relaxed),
            response_fallback: inner
                .udp_responses_priority_reserved
                .load(Ordering::Relaxed),
            response_rate_limited: inner.udp_responses_rate_limited.load(Ordering::Relaxed),
            response_send_failed: inner.response_send_failed.load(Ordering::Relaxed),
            announce_accepted: inner.announce_accepted.load(Ordering::Relaxed),
            announce_invalid_token: inner.announce_invalid_token.load(Ordering::Relaxed),
            announce_filtered: inner.announce_filtered.load(Ordering::Relaxed),
            node_admitted: inner.node_admitted.load(Ordering::Relaxed),
            node_replaced: inner.node_replaced.load(Ordering::Relaxed),
            node_dropped_duplicate: inner.node_dropped_duplicate.load(Ordering::Relaxed),
            node_dropped_rate_limited: inner.node_dropped_rate_limited.load(Ordering::Relaxed),
            node_dropped_invalid: inner.node_dropped_invalid.load(Ordering::Relaxed),
            metadata_bytes_downloaded: inner.metadata_bytes_downloaded.load(Ordering::Relaxed),
            metadata_failure_timeout: inner.metadata_failure_timeout.load(Ordering::Relaxed),
            metadata_failure_connect: inner.metadata_failure_connect.load(Ordering::Relaxed),
            metadata_failure_no_extension: inner
                .metadata_failure_no_extension
                .load(Ordering::Relaxed),
            metadata_failure_send: inner.metadata_failure_send.load(Ordering::Relaxed),
            metadata_failure_size_limit: inner.metadata_failure_size_limit.load(Ordering::Relaxed),
            metadata_failure_sha1: inner.metadata_failure_sha1.load(Ordering::Relaxed),
            metadata_failure_parse: inner.metadata_failure_parse.load(Ordering::Relaxed),
            metadata_failure_other: inner.metadata_failure_other.load(Ordering::Relaxed),
            peer_cache_timeout_hits: inner.peer_cache_timeout_hits.load(Ordering::Relaxed),
            peer_cache_connect_hits: inner.peer_cache_connect_hits.load(Ordering::Relaxed),
            queue_wait_ms: inner.queue_wait_ms.snapshot(),
            fetch_duration_ms: inner.fetch_duration_ms.snapshot(),
            metadata_size_bytes: inner.metadata_size_bytes.snapshot(),
        }
    }

    pub(crate) fn hash_received(&self) {
        self.inner.hashes_received.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn hash_ingress_dropped(&self) {
        self.inner
            .hash_ingress_dropped
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn set_hash_ingress_queue_depth(&self, depth: usize) {
        self.inner
            .hash_ingress_queue_depth
            .store(depth, Ordering::Relaxed);
    }

    pub(crate) fn set_crawl_priority_queue_depth(&self, depth: usize) {
        self.inner
            .crawl_priority_queue_depth
            .store(depth, Ordering::Relaxed);
    }

    pub(crate) fn set_crawl_discovery_queue_depth(&self, depth: usize) {
        self.inner
            .crawl_discovery_queue_depth
            .store(depth, Ordering::Relaxed);
    }

    pub(crate) fn set_metadata_queue(&self, depth: usize, in_flight: usize) {
        self.inner
            .metadata_queue_depth
            .store(depth, Ordering::Relaxed);
        self.inner
            .metadata_in_flight
            .store(in_flight, Ordering::Relaxed);
    }

    pub(crate) fn metadata_queue_deduplicated(&self) {
        self.inner
            .metadata_queue_deduplicated
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metadata_queue_inserted(&self) {
        self.inner
            .metadata_queue_inserted
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metadata_queue_evicted(&self) {
        self.inner
            .metadata_queue_evicted
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metadata_queue_stale(&self, count: usize) {
        self.inner
            .metadata_queue_stale
            .fetch_add(count as u64, Ordering::Relaxed);
    }

    pub(crate) fn metadata_race_started(&self) {
        self.inner
            .metadata_races_started
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metadata_peer_candidate(&self) {
        self.inner
            .metadata_peer_candidates
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metadata_live_peer_join(&self) {
        self.inner
            .metadata_live_peer_joins
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metadata_peer_canceled(&self, count: usize) {
        self.inner
            .metadata_peer_canceled
            .fetch_add(count as u64, Ordering::Relaxed);
    }

    pub(crate) fn peer_lookup_requested(&self) {
        self.inner
            .peer_lookup_requested
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn peer_lookup_started(&self) {
        self.inner
            .peer_lookup_started
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn peer_lookup_rate_limited(&self) {
        self.inner
            .peer_lookup_rate_limited
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn peer_lookup_empty(&self) {
        self.inner.peer_lookup_empty.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn peer_lookup_query(&self) {
        self.inner
            .peer_lookup_queries
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn peer_lookup_response(&self) {
        self.inner
            .peer_lookup_responses
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn peer_lookup_timeout(&self) {
        self.inner
            .peer_lookup_timeouts
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn peer_lookup_send_failed(&self) {
        self.inner
            .peer_lookup_send_failures
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn peer_lookup_peer_found(&self) {
        self.inner
            .peer_lookup_peers_found
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn peer_lookup_response_dropped(&self) {
        self.inner
            .peer_lookup_response_dropped
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn peer_lookup_output_dropped(&self) {
        self.inner
            .peer_lookup_output_dropped
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metadata_peer_attempt(&self) {
        self.inner
            .metadata_peer_attempts
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metadata_peer_succeeded(&self) {
        self.inner
            .metadata_peer_succeeded
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metadata_peer_failed(&self) {
        self.inner
            .metadata_peer_failed
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metadata_peer_timeout(&self) {
        self.inner
            .metadata_peer_timeouts
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metadata_connect_failed(&self) {
        self.inner
            .metadata_connect_failed
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metadata_no_extension(&self) {
        self.inner
            .metadata_no_extension
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metadata_peer_failure_cache_hit(&self) {
        self.inner
            .metadata_peer_failure_cache_hits
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn set_metadata_peer_failure_cache_entries(&self, count: usize) {
        self.inner
            .metadata_peer_failure_cache_entries
            .store(count, Ordering::Relaxed);
    }

    pub(crate) fn set_node_pool_size(&self, size: usize) {
        self.inner.node_pool_size.store(size, Ordering::Relaxed);
    }

    pub(crate) fn set_find_node_in_flight(&self, count: usize) {
        self.inner
            .find_node_in_flight
            .store(count, Ordering::Relaxed);
    }

    pub(crate) fn set_find_node_effective_rate(&self, rate: u32) {
        self.inner
            .find_node_effective_rate_per_sec
            .store(rate, Ordering::Relaxed);
    }

    pub(crate) fn query_new(&self) {
        self.inner.queries_new.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn query_revisit(&self) {
        self.inner.queries_revisit.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn query_bootstrap(&self) {
        self.inner.queries_bootstrap.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn response(&self) {
        self.inner.responses.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn unmatched_response(&self) {
        self.inner
            .unmatched_responses
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn timeout(&self) {
        self.inner.timeouts.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn send_failure(&self) {
        self.inner.send_failures.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn crawl_event_dropped_response(&self) {
        self.inner
            .crawl_events_dropped_response
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn crawl_event_dropped_discovered(&self) {
        self.inner
            .crawl_events_dropped_discovered
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn udp_received(&self) {
        self.inner.udp_received.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn udp_received_bytes(&self, bytes: usize) {
        self.inner
            .udp_rx_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub(crate) fn udp_sent(&self, bytes: usize) {
        self.inner.udp_tx_packets.fetch_add(1, Ordering::Relaxed);
        self.inner
            .udp_tx_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub(crate) fn inbound_query(&self, query: &str) {
        let counter = match query {
            "ping" => &self.inner.inbound_ping,
            "find_node" => &self.inner.inbound_find_node,
            "get_peers" => &self.inner.inbound_get_peers,
            "announce_peer" => &self.inner.inbound_announce_peer,
            _ => &self.inner.inbound_other,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn response_normal(&self) {
        self.inner.response_normal.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn response_send_failed(&self) {
        self.inner
            .response_send_failed
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn announce_accepted(&self) {
        self.inner.announce_accepted.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn announce_invalid_token(&self) {
        self.inner
            .announce_invalid_token
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn announce_filtered(&self) {
        self.inner.announce_filtered.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn node_admitted(&self) {
        self.inner.node_admitted.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn node_replaced(&self) {
        self.inner.node_replaced.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn node_dropped_duplicate(&self) {
        self.inner
            .node_dropped_duplicate
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn node_dropped_rate_limited(&self) {
        self.inner
            .node_dropped_rate_limited
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn node_dropped_invalid(&self) {
        self.inner
            .node_dropped_invalid
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metadata_bytes_downloaded(&self, bytes: usize) {
        self.inner
            .metadata_bytes_downloaded
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub(crate) fn metadata_failure_timeout(&self) {
        self.inner
            .metadata_failure_timeout
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metadata_failure_connect(&self) {
        self.inner
            .metadata_failure_connect
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metadata_failure_no_extension(&self) {
        self.inner
            .metadata_failure_no_extension
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metadata_failure_send(&self) {
        self.inner
            .metadata_failure_send
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metadata_failure_size_limit(&self) {
        self.inner
            .metadata_failure_size_limit
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metadata_failure_sha1(&self) {
        self.inner
            .metadata_failure_sha1
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metadata_failure_parse(&self) {
        self.inner
            .metadata_failure_parse
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn metadata_failure_other(&self) {
        self.inner
            .metadata_failure_other
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn peer_cache_hit_timeout(&self) {
        self.inner
            .peer_cache_timeout_hits
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn peer_cache_hit_connect(&self) {
        self.inner
            .peer_cache_connect_hits
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn observe_metadata_queue_wait(&self, millis: u64) {
        self.inner.queue_wait_ms.record(millis);
    }

    pub(crate) fn observe_metadata_fetch_duration(&self, millis: u64) {
        self.inner.fetch_duration_ms.record(millis);
    }

    pub(crate) fn observe_metadata_size(&self, bytes: usize) {
        self.inner.metadata_size_bytes.record(bytes as u64);
    }

    pub(crate) fn udp_queue_full(&self) {
        self.inner.udp_queue_full.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn udp_invalid(&self) {
        self.inner.udp_invalid.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn udp_response_rate_limited(&self) {
        self.inner
            .udp_responses_rate_limited
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn udp_response_priority_reserved(&self) {
        self.inner
            .udp_responses_priority_reserved
            .fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloned_handle_updates_one_snapshot() {
        let stats = DhtRuntimeStats::with_limits(DhtRuntimeLimits {
            metadata_queue: 100,
            node_pool: 1_000,
            node_pool_low_watermark: 10,
            find_node_in_flight: 512,
            initial_find_node_rate: 200,
            hash_ingress_queue: 20,
            crawl_priority_queue: 30,
            crawl_discovery_queue: 40,
        });
        let writer = stats.clone();

        writer.hash_received();
        writer.hash_ingress_dropped();
        writer.set_hash_ingress_queue_depth(11);
        writer.set_crawl_priority_queue_depth(12);
        writer.set_crawl_discovery_queue_depth(13);
        writer.set_metadata_queue(42, 7);
        writer.metadata_queue_inserted();
        writer.metadata_queue_deduplicated();
        writer.metadata_queue_evicted();
        writer.metadata_queue_stale(3);
        writer.metadata_race_started();
        writer.metadata_peer_candidate();
        writer.metadata_live_peer_join();
        writer.metadata_peer_canceled(2);
        writer.peer_lookup_requested();
        writer.peer_lookup_started();
        writer.peer_lookup_rate_limited();
        writer.peer_lookup_empty();
        writer.peer_lookup_query();
        writer.peer_lookup_response();
        writer.peer_lookup_timeout();
        writer.peer_lookup_send_failed();
        writer.peer_lookup_peer_found();
        writer.peer_lookup_response_dropped();
        writer.peer_lookup_output_dropped();
        writer.metadata_peer_attempt();
        writer.metadata_peer_succeeded();
        writer.metadata_peer_failed();
        writer.metadata_peer_timeout();
        writer.metadata_connect_failed();
        writer.metadata_no_extension();
        writer.metadata_peer_failure_cache_hit();
        writer.set_metadata_peer_failure_cache_entries(9);
        writer.set_node_pool_size(321);
        writer.set_find_node_in_flight(12);
        writer.set_find_node_effective_rate(150);
        writer.query_new();
        writer.query_revisit();
        writer.query_bootstrap();
        writer.response();
        writer.unmatched_response();
        writer.timeout();
        writer.send_failure();
        writer.crawl_event_dropped_response();
        writer.crawl_event_dropped_discovered();
        writer.udp_received();
        writer.udp_queue_full();
        writer.udp_invalid();
        writer.udp_response_rate_limited();
        writer.udp_response_priority_reserved();

        assert_eq!(
            stats.snapshot(),
            DhtRuntimeSnapshot {
                hashes_received: 1,
                hash_ingress_dropped: 1,
                hash_ingress_queue_depth: 11,
                hash_ingress_queue_capacity: 20,
                crawl_priority_queue_depth: 12,
                crawl_priority_queue_capacity: 30,
                crawl_discovery_queue_depth: 13,
                crawl_discovery_queue_capacity: 40,
                metadata_queue_depth: 42,
                metadata_queue_max: 100,
                metadata_in_flight: 7,
                metadata_queue_inserted: 1,
                metadata_queue_deduplicated: 1,
                metadata_queue_evicted: 1,
                metadata_queue_stale: 3,
                metadata_races_started: 1,
                metadata_peer_candidates: 1,
                metadata_live_peer_joins: 1,
                metadata_peer_canceled: 2,
                peer_lookup_requested: 1,
                peer_lookup_started: 1,
                peer_lookup_rate_limited: 1,
                peer_lookup_empty: 1,
                peer_lookup_queries: 1,
                peer_lookup_responses: 1,
                peer_lookup_timeouts: 1,
                peer_lookup_send_failures: 1,
                peer_lookup_peers_found: 1,
                peer_lookup_response_dropped: 1,
                peer_lookup_output_dropped: 1,
                metadata_peer_attempts: 1,
                metadata_peer_succeeded: 1,
                metadata_peer_failed: 1,
                metadata_peer_timeouts: 1,
                metadata_connect_failed: 1,
                metadata_no_extension: 1,
                metadata_peer_failure_cache_hits: 1,
                metadata_peer_failure_cache_entries: 9,
                node_pool_size: 321,
                node_pool_capacity: 1_000,
                node_pool_low_watermark: 10,
                find_node_in_flight: 12,
                find_node_in_flight_max: 512,
                find_node_effective_rate_per_sec: 150,
                queries_new: 1,
                queries_revisit: 1,
                queries_bootstrap: 1,
                responses: 1,
                unmatched_responses: 1,
                timeouts: 1,
                send_failures: 1,
                crawl_events_dropped_response: 1,
                crawl_events_dropped_discovered: 1,
                udp_received: 1,
                udp_queue_full: 1,
                udp_invalid: 1,
                udp_responses_rate_limited: 1,
                udp_responses_priority_reserved: 1,
            }
        );
    }

    #[test]
    fn observability_snapshot_tracks_fixed_histograms_and_categories() {
        let stats = DhtRuntimeStats::default();
        stats.udp_received();
        stats.udp_received_bytes(128);
        stats.udp_sent(64);
        stats.inbound_query("ping");
        stats.inbound_query("unknown");
        stats.node_admitted();
        stats.metadata_bytes_downloaded(1_024);
        stats.metadata_failure_parse();
        stats.observe_metadata_queue_wait(75);
        stats.observe_metadata_queue_wait(600);
        stats.observe_metadata_fetch_duration(1_500);
        stats.observe_metadata_size(70_000);

        let snapshot = stats.observability_snapshot();
        assert_eq!(snapshot.udp_rx_packets, 1);
        assert_eq!(snapshot.udp_rx_bytes, 128);
        assert_eq!(snapshot.udp_tx_packets, 1);
        assert_eq!(snapshot.udp_tx_bytes, 64);
        assert_eq!(snapshot.inbound_ping, 1);
        assert_eq!(snapshot.inbound_other, 1);
        assert_eq!(snapshot.node_admitted, 1);
        assert_eq!(snapshot.metadata_bytes_downloaded, 1_024);
        assert_eq!(snapshot.metadata_failure_parse, 1);
        assert_eq!(snapshot.queue_wait_ms.percentile(0.50), Some(100));
        assert_eq!(snapshot.queue_wait_ms.percentile(0.95), Some(1_000));
        assert_eq!(snapshot.fetch_duration_ms.percentile(0.95), Some(2_000));
        assert_eq!(snapshot.metadata_size_bytes.percentile(0.50), Some(131_072));
    }
}
