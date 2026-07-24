use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
/// IP families on which the DHT server listens and crawls.
pub enum NetMode {
    /// Bind and crawl IPv4 only.
    Ipv4Only,
    /// Bind and crawl IPv6 only.
    Ipv6Only,
    #[default]
    /// Bind separate IPv4 and IPv6 sockets.
    DualStack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Validated torrent metadata delivered to the application callback.
pub struct TorrentInfo {
    /// Lowercase hexadecimal SHA1 of the bencoded info dictionary.
    pub info_hash: String,
    /// Magnet URI containing the InfoHash.
    pub magnet_link: String,
    /// Torrent display name.
    pub name: String,
    /// Sum of file sizes in bytes.
    pub total_size: u64,
    /// Files described by the torrent.
    pub files: Vec<FileInfo>,
    /// Torrent piece length in bytes, or zero if absent.
    pub piece_length: u64,
    /// Peer addresses used to obtain the Metadata.
    pub peers: Vec<String>,
    /// Completion time as Unix seconds.
    pub timestamp: u64,
}

/// Final outcome of a metadata fetch that passed the admission callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataFetchCompletionStatus {
    /// Metadata was fetched and accepted by the torrent callback.
    Accepted,
    /// All available peer candidates failed.
    FetchFailed,
    /// Metadata was fetched, but the application did not accept it.
    DeliveryRejected,
}

/// Report emitted exactly once after an admitted metadata fetch finishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataFetchCompletion {
    /// InfoHash that reached a terminal state.
    pub info_hash: String,
    /// Final download/delivery status.
    pub status: MetadataFetchCompletionStatus,
    /// Real Peer network attempts; failure-cache skips are excluded.
    pub attempts: usize,
}

impl MetadataFetchCompletion {
    /// Returns true only for [`MetadataFetchCompletionStatus::Accepted`].
    pub fn is_success(&self) -> bool {
        self.status == MetadataFetchCompletionStatus::Accepted
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// One file entry from the validated info dictionary.
pub struct FileInfo {
    /// Slash-separated relative path.
    pub path: String,
    /// File size in bytes.
    pub size: u64,
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
/// Compact DHT node tuple used by crawl and routing code.
pub struct NodeTuple {
    /// Twenty-byte DHT node ID.
    pub id: [u8; 20],
    /// Public UDP endpoint.
    pub addr: SocketAddr,
}

impl TorrentInfo {
    /// Formats [`Self::total_size`] using binary thresholds and a short unit suffix.
    pub fn format_size(&self) -> String {
        format_bytes(self.total_size)
    }
}

impl FileInfo {
    /// Formats [`Self::size`] using binary thresholds and a short unit suffix.
    pub fn format_size(&self) -> String {
        format_bytes(self.size)
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_index = 0;
    while size >= 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }
    format!("{size:.2} {}", UNITS[unit_index])
}

#[derive(Debug, Clone)]
/// Complete server configuration.
pub struct DHTOptions {
    /// UDP listen port.
    pub port: u16,
    /// Enabled IP families.
    pub netmode: NetMode,
    /// Capacity between announce processing and the Metadata scheduler.
    pub hash_queue_capacity: usize,
    /// Metadata download and Peer-cache limits.
    pub metadata: MetadataOptions,
    /// Active get_peers lookup rate and concurrency limits.
    pub peer_lookup: PeerLookupOptions,
    /// Active crawl, node-pool and scheduler limits.
    pub crawl: CrawlOptions,
}

#[derive(Debug, Clone)]
/// Metadata download and failure-cache limits.
pub struct MetadataOptions {
    /// End-to-end timeout for one Peer attempt, in seconds.
    pub timeout_secs: u64,
    /// Maximum number of deduplicated pending InfoHashes.
    pub max_queue_size: usize,
    /// Maximum number of concurrent Metadata jobs.
    pub max_worker_count: usize,
    /// Maximum number of cached bad Peer socket addresses.
    pub peer_failure_cache_capacity: usize,
    /// Timeout/connect failure cache lifetime in seconds.
    pub peer_failure_ttl_secs: u64,
}

#[derive(Debug, Clone)]
/// Active get_peers lookup budgets used to discover additional Metadata Peers.
pub struct PeerLookupOptions {
    /// Maximum new InfoHash lookups started per second. Zero disables active lookup.
    pub max_lookups_per_second: u32,
    /// Maximum lookup budget consumed immediately after an idle period.
    pub burst: u32,
    /// Maximum InfoHash lookups kept active at the same time.
    pub max_active_lookups: usize,
}

#[derive(Debug, Clone, Default)]
/// Active crawl configuration grouped by responsibility.
pub struct CrawlOptions {
    /// FIFO node-pool and responsive-ring limits.
    pub pool: PoolOptions,
    /// Query, replacement and response budgets.
    pub rate_limit: RateLimitOptions,
    /// Bootstrap sources and retry policy.
    pub bootstrap: BootstrapOptions,
    /// Target-generation policy.
    pub target: TargetOptions,
    /// Internal bounded-channel and snapshot limits.
    pub scheduler: SchedulerOptions,
}

#[derive(Debug, Clone)]
/// Independent crawl and UDP-response budgets.
pub struct RateLimitOptions {
    /// Maximum active find_node queries scheduled per second.
    pub max_find_node_rate_per_sec: u32,
    /// Maximum query budget consumed in one scheduler tick.
    pub burst: u32,
    /// Maximum total pending find_node transactions.
    pub max_in_flight: usize,
    /// Pending find_node timeout in seconds.
    pub request_timeout_secs: u64,
    /// Maximum never-before-probed destinations per minute.
    pub max_new_destinations_per_minute: u32,
    /// Maximum outbound DHT response packets per second.
    pub max_response_rate_per_sec: u32,
    /// Maximum encoded outbound DHT response bytes per second.
    pub max_response_bytes_per_sec: u64,
    /// Maximum response packets per source address per second.
    pub max_response_rate_per_source: u32,
    /// Remaining query-rate percentage when Metadata pressure reaches 95%.
    pub metadata_pressure_floor_percent: u8,
    /// Maximum FIFO replacements per minute after the pool has warmed.
    pub max_replacements_per_minute: u32,
    /// Maximum pending find_node transactions per IP subnet.
    pub max_in_flight_per_subnet: usize,
}

#[derive(Debug, Clone)]
/// FIFO crawl-pool and responsive-node reservoir limits.
pub struct PoolOptions {
    /// Maximum queued crawl nodes.
    pub capacity: usize,
    /// How long a probed endpoint is blocked from readmission, in seconds.
    pub recent_probe_ttl_secs: u64,
    /// Maximum nodes retained for replies and revisit traffic.
    pub responsive_capacity: usize,
    /// Responsive-node lifetime in seconds.
    pub responsive_ttl_secs: u64,
    /// Pool size below which bootstrap is considered.
    pub low_watermark: usize,
}

#[derive(Debug, Clone)]
/// Bootstrap hostnames and retry timing.
pub struct BootstrapOptions {
    /// Host:port sources resolved when bootstrap is needed.
    pub nodes: Vec<String>,
    /// Minimum interval between bootstrap rounds, in seconds.
    pub interval_secs: u64,
    /// Maximum resolved endpoints selected in one round.
    pub max_nodes_per_round: usize,
    /// Initial failed-source backoff in seconds.
    pub source_backoff_base_secs: u64,
    /// Maximum failed-source backoff in seconds.
    pub source_backoff_max_secs: u64,
}

#[derive(Debug, Clone)]
/// Distribution used to generate find_node targets and sender IDs.
pub struct TargetOptions {
    /// Percentage of targets that are fully random.
    pub random_walk_percent: u8,
    /// Percentage of targets chosen from sparse routing buckets.
    pub sparse_bucket_percent: u8,
    /// Whether outbound sender IDs borrow the target's prefix.
    pub neighbor_sender_id: bool,
}

#[derive(Debug, Clone)]
/// Capacities and batch limits for the crawl actor.
pub struct SchedulerOptions {
    /// Capacity for response/bootstrap priority events.
    pub priority_event_channel_capacity: usize,
    /// Capacity for newly discovered node events.
    pub discovery_event_channel_capacity: usize,
    /// Maximum events drained per actor iteration.
    pub event_batch_limit: usize,
    /// Maximum discovery nodes drained per actor iteration.
    pub node_batch_limit: usize,
    /// Maximum responsive nodes published in the lock-free snapshot.
    pub routing_snapshot_size: usize,
    /// Snapshot publication interval in milliseconds.
    pub snapshot_refresh_millis: u64,
}

impl Default for DHTOptions {
    fn default() -> Self {
        Self {
            port: 6881,
            netmode: NetMode::Ipv4Only,
            hash_queue_capacity: 10_000,
            metadata: MetadataOptions::default(),
            peer_lookup: PeerLookupOptions::default(),
            crawl: CrawlOptions::default(),
        }
    }
}

impl Default for MetadataOptions {
    fn default() -> Self {
        Self {
            timeout_secs: 4,
            max_queue_size: 10_000,
            max_worker_count: 256,
            peer_failure_cache_capacity: 200_000,
            peer_failure_ttl_secs: 60,
        }
    }
}

impl Default for PeerLookupOptions {
    fn default() -> Self {
        Self {
            max_lookups_per_second: 32,
            burst: 32,
            max_active_lookups: 64,
        }
    }
}

impl Default for RateLimitOptions {
    fn default() -> Self {
        Self {
            max_find_node_rate_per_sec: 200,
            burst: 40,
            max_in_flight: 512,
            request_timeout_secs: 2,
            max_new_destinations_per_minute: 10_000,
            max_response_rate_per_sec: 500,
            max_response_bytes_per_sec: 1024 * 1024,
            max_response_rate_per_source: 40,
            metadata_pressure_floor_percent: 25,
            max_replacements_per_minute: 25_000,
            max_in_flight_per_subnet: 8,
        }
    }
}

impl Default for PoolOptions {
    fn default() -> Self {
        Self {
            capacity: 100_000,
            recent_probe_ttl_secs: 600,
            responsive_capacity: 16_384,
            responsive_ttl_secs: 900,
            low_watermark: 10_000,
        }
    }
}

impl Default for BootstrapOptions {
    fn default() -> Self {
        Self {
            nodes: vec![
                "router.bittorrent.com:6881".to_string(),
                "dht.transmissionbt.com:6881".to_string(),
                "router.utorrent.com:6881".to_string(),
                "dht.aelitis.com:6881".to_string(),
            ],
            interval_secs: 300,
            max_nodes_per_round: 3,
            source_backoff_base_secs: 300,
            source_backoff_max_secs: 3_600,
        }
    }
}

impl Default for TargetOptions {
    fn default() -> Self {
        Self {
            random_walk_percent: 70,
            sparse_bucket_percent: 30,
            neighbor_sender_id: true,
        }
    }
}

impl Default for SchedulerOptions {
    fn default() -> Self {
        Self {
            priority_event_channel_capacity: 8_192,
            discovery_event_channel_capacity: 16_384,
            event_batch_limit: 256,
            node_batch_limit: 4_096,
            routing_snapshot_size: 4_096,
            snapshot_refresh_millis: 1_000,
        }
    }
}
