use crate::addr::is_valid_node_addr;
use crate::budget::RateBucket;
use crate::crawl_config::ResolvedCrawlConfig;
use crate::crawl_engine::CrawlEngine;
use crate::error::Result;
use crate::krpc::encode_response;
use crate::metadata::RbitFetcher;
use crate::node_id::{neighbor_node_id, random_node_id, transaction_id_from_bytes};
use crate::peer_lookup::{
    PeerLookupHandle, PeerLookupRuntime, is_peer_lookup_tid, spawn_peer_lookup,
};
use crate::protocol::{DhtArgs, DhtMessage};
use crate::runtime_stats::{DhtRuntimeLimits, DhtRuntimeStats};
use crate::scheduler::{
    MetadataCompletionCallback, MetadataFetchCallback, MetadataScheduler,
    MetadataSchedulerCallbacks, MetadataSchedulerLimits, MetadataSchedulerRuntime,
    TorrentAckCallback,
};
use crate::types::{DHTOptions, MetadataFetchCompletion, NetMode, NodeTuple, TorrentInfo};
use crate::udp_buffer::UdpBufferPool;
use crate::udp_ingress::{WorkerHandle, spawn_udp_listener};
use ahash::AHashMap;
use arc_swap::ArcSwapOption;
use bytes::BytesMut;
#[cfg(feature = "metrics")]
use metrics::counter;
use rand::Rng;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

type FilterCallback = Box<dyn Fn(&str) -> bool + Send + Sync + 'static>;
type ErrorCallback = Box<dyn Fn(crate::error::DHTError) + Send + Sync + 'static>;

struct QueryResponse<'a> {
    transaction_id: &'a [u8],
    remote_addr: SocketAddr,
    local_addr: SocketAddr,
    query_type: &'a str,
    sender_id: Option<&'a [u8]>,
    target_id: Option<&'a [u8]>,
}

#[derive(Clone, Copy)]
struct SourceResponseWindow {
    started_at: Instant,
    last_seen: Instant,
    count: u32,
}

struct WorkerResponseLimiter {
    regular_packets: RateBucket,
    regular_bytes: RateBucket,
    priority_packets: RateBucket,
    priority_bytes: RateBucket,
    per_source_rate: u32,
    sources: AHashMap<SocketAddr, SourceResponseWindow>,
    source_expiry: VecDeque<(Instant, SocketAddr)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponsePermit {
    Regular,
    PriorityReserve,
    Rejected,
}

fn reserve_quota(total: u32) -> u32 {
    if total == 0 {
        0
    } else {
        total.div_ceil(10).min(total)
    }
}

impl WorkerResponseLimiter {
    fn new(packet_rate: u32, byte_rate: u64, per_source_rate: u32, now: Instant) -> Self {
        let byte_rate = byte_rate.min(u32::MAX as u64) as u32;
        let priority_packet_rate = reserve_quota(packet_rate);
        let priority_byte_rate = reserve_quota(byte_rate);
        let packet_rate = packet_rate.saturating_sub(priority_packet_rate);
        let byte_rate = byte_rate.saturating_sub(priority_byte_rate);
        Self {
            regular_packets: RateBucket::per_second(
                packet_rate,
                packet_rate.div_ceil(5).max(1),
                true,
                now,
            ),
            regular_bytes: RateBucket::per_second(
                byte_rate,
                byte_rate.div_ceil(5).max(512),
                true,
                now,
            ),
            priority_packets: RateBucket::per_second(
                priority_packet_rate,
                priority_packet_rate.div_ceil(5).max(1),
                true,
                now,
            ),
            priority_bytes: RateBucket::per_second(
                priority_byte_rate,
                priority_byte_rate.div_ceil(5).max(512),
                true,
                now,
            ),
            per_source_rate,
            sources: AHashMap::new(),
            source_expiry: VecDeque::new(),
        }
    }

    fn acquire(
        &mut self,
        addr: SocketAddr,
        encoded_len: usize,
        is_priority: bool,
        now: Instant,
    ) -> ResponsePermit {
        self.expire_sources(now);
        if Self::take_budget(
            &mut self.regular_packets,
            &mut self.regular_bytes,
            encoded_len,
            now,
        ) {
            if self.acquire_source_slot(addr, now) {
                return ResponsePermit::Regular;
            }
            Self::refund_budget(
                &mut self.regular_packets,
                &mut self.regular_bytes,
                encoded_len,
            );
            return ResponsePermit::Rejected;
        }

        if is_priority
            && Self::take_budget(
                &mut self.priority_packets,
                &mut self.priority_bytes,
                encoded_len,
                now,
            )
        {
            if self.acquire_source_slot(addr, now) {
                return ResponsePermit::PriorityReserve;
            }
            Self::refund_budget(
                &mut self.priority_packets,
                &mut self.priority_bytes,
                encoded_len,
            );
        }

        ResponsePermit::Rejected
    }

    fn take_budget(
        packets: &mut RateBucket,
        bytes: &mut RateBucket,
        encoded_len: usize,
        now: Instant,
    ) -> bool {
        if !packets.try_take_one(now) {
            return false;
        }
        if !bytes.try_take_exact(encoded_len, now) {
            packets.refund_one();
            return false;
        }
        true
    }

    fn refund_budget(packets: &mut RateBucket, bytes: &mut RateBucket, encoded_len: usize) {
        packets.refund_one();
        bytes.refund(encoded_len);
    }

    fn acquire_source_slot(&mut self, addr: SocketAddr, now: Instant) -> bool {
        if self.per_source_rate == 0 {
            return false;
        }

        let entry = self.sources.entry(addr).or_insert(SourceResponseWindow {
            started_at: now,
            last_seen: now,
            count: 0,
        });
        if now
            .checked_duration_since(entry.started_at)
            .unwrap_or_default()
            >= Duration::from_secs(1)
        {
            entry.started_at = now;
            entry.count = 0;
        }
        if entry.count >= self.per_source_rate {
            return false;
        }
        entry.count += 1;
        entry.last_seen = now;
        self.source_expiry
            .push_back((now + Duration::from_secs(60), addr));
        true
    }

    fn expire_sources(&mut self, now: Instant) {
        while let Some((deadline, addr)) = self.source_expiry.front().copied() {
            if deadline > now {
                break;
            }
            self.source_expiry.pop_front();
            if self.sources.get(&addr).is_some_and(|entry| {
                now.checked_duration_since(entry.last_seen)
                    .unwrap_or_default()
                    >= Duration::from_secs(60)
            }) {
                self.sources.remove(&addr);
            }
        }
    }
}

#[derive(Debug, Clone)]
/// InfoHash and announcing Peer submitted to the Metadata scheduler.
pub struct HashDiscovered {
    /// Lowercase hexadecimal InfoHash.
    pub info_hash: String,
    /// Peer endpoint derived from announce `port`/`implied_port`.
    pub peer_addr: SocketAddr,
    /// Monotonic discovery time used for freshness and queue ordering.
    pub discovered_at: std::time::Instant,
}

#[derive(Clone)]
/// Cloneable BEP-5 server handle and primary crate entry point.
pub struct DHTServer {
    options: DHTOptions,
    crawl_config: ResolvedCrawlConfig,
    node_id: [u8; 20],
    sockets_by_bind_addr: Arc<HashMap<SocketAddr, Arc<UdpSocket>>>,
    token_secret: [u8; 10],
    torrent_callback: Arc<ArcSwapOption<TorrentAckCallback>>,
    hash_filter: Arc<ArcSwapOption<FilterCallback>>,
    on_metadata_fetch: Arc<ArcSwapOption<MetadataFetchCallback>>,
    metadata_completion_callback: Arc<ArcSwapOption<MetadataCompletionCallback>>,
    error_callback: Arc<ArcSwapOption<ErrorCallback>>,
    crawl_engine: Arc<CrawlEngine>,
    peer_lookup: PeerLookupHandle,
    hash_events_tx: mpsc::Sender<HashDiscovered>,
    metadata_queue_len: Arc<AtomicUsize>,
    max_metadata_queue_size: usize,
    runtime_stats: DhtRuntimeStats,
    shutdown: CancellationToken,
}

fn create_udp_sock(domain: Domain, ty: Type, addr: SocketAddr) -> std::io::Result<UdpSocket> {
    let sock = Socket::new(domain, ty, Some(Protocol::UDP))?;
    #[cfg(not(windows))]
    {
        sock.set_reuse_port(true)?;
        if addr.is_ipv6() {
            sock.set_only_v6(true)?;
        }
    }
    let _ = sock.set_reuse_address(true);
    sock.set_nonblocking(true)?;
    let _ = sock.set_recv_buffer_size(32 * 1024 * 1024);
    let _ = sock.set_send_buffer_size(8 * 1024 * 1024);
    sock.bind(&addr.into())?;
    UdpSocket::from_std(sock.into())
}

fn split_u32_quota(total: u32, workers: usize, worker: usize) -> u32 {
    let workers = workers.max(1) as u32;
    total / workers + u32::from((worker as u32) < total % workers)
}

fn split_u64_quota(total: u64, workers: usize, worker: usize) -> u64 {
    let workers = workers.max(1) as u64;
    total / workers + u64::from((worker as u64) < total % workers)
}

impl DHTServer {
    /// Validates options, binds configured UDP sockets and constructs bounded pipelines.
    ///
    /// Background Metadata scheduling begins during construction. Active crawling and UDP receive
    /// loops begin when [`Self::start`] is awaited.
    pub async fn new(options: DHTOptions) -> Result<Self> {
        let crawl_config = ResolvedCrawlConfig::from_options(&options.crawl);
        let runtime_stats = DhtRuntimeStats::with_limits(DhtRuntimeLimits {
            metadata_queue: options.metadata.max_queue_size.max(1),
            node_pool: crawl_config.pool_capacity,
            node_pool_low_watermark: crawl_config.low_watermark,
            find_node_in_flight: crawl_config.max_in_flight,
            initial_find_node_rate: crawl_config.max_find_node_rate_per_sec,
            hash_ingress_queue: options.hash_queue_capacity,
            crawl_priority_queue: crawl_config.priority_event_channel_capacity,
            crawl_discovery_queue: crawl_config.discovery_event_channel_capacity,
        });
        const ANY_V4_ADDR: SocketAddr =
            SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 8080);
        const ANY_V6_ADDR: SocketAddr =
            SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0)), 8080);

        let mut sockets_by_bind_addr = HashMap::new();
        match options.netmode {
            NetMode::Ipv4Only => {
                let mut addr = ANY_V4_ADDR;
                addr.set_port(options.port);
                let sock = create_udp_sock(Domain::IPV4, Type::DGRAM, addr)?;
                sockets_by_bind_addr.insert(addr, Arc::new(sock));
            }
            NetMode::Ipv6Only => {
                let mut addr = ANY_V6_ADDR;
                addr.set_port(options.port);
                let sock = create_udp_sock(Domain::IPV6, Type::DGRAM, addr)?;
                sockets_by_bind_addr.insert(addr, Arc::new(sock));
            }
            NetMode::DualStack => {
                let mut addr_v4 = ANY_V4_ADDR;
                addr_v4.set_port(options.port);
                let sock_v4 = create_udp_sock(Domain::IPV4, Type::DGRAM, addr_v4)?;
                sockets_by_bind_addr.insert(addr_v4, Arc::new(sock_v4));

                let mut addr_v6 = ANY_V6_ADDR;
                addr_v6.set_port(options.port);
                let sock_v6 = create_udp_sock(Domain::IPV6, Type::DGRAM, addr_v6)?;
                sockets_by_bind_addr.insert(addr_v6, Arc::new(sock_v6));
            }
        }

        let node_id = random_node_id();
        let mut token_secret = [0u8; 10];
        rand::thread_rng().fill(&mut token_secret);

        let (hash_events_tx, hash_rx) =
            mpsc::channel::<HashDiscovered>(options.hash_queue_capacity);
        let fetcher = Arc::new(RbitFetcher::new_with_runtime_stats(
            options.metadata.timeout_secs,
            options.metadata.peer_failure_cache_capacity,
            options.metadata.peer_failure_ttl_secs,
            runtime_stats.clone(),
        ));
        let torrent_callback = Arc::new(ArcSwapOption::empty());
        let on_metadata_fetch = Arc::new(ArcSwapOption::empty());
        let metadata_completion_callback = Arc::new(ArcSwapOption::empty());
        let metadata_queue_len = Arc::new(AtomicUsize::new(0));
        let shutdown = CancellationToken::new();
        let crawl_engine = Arc::new(CrawlEngine::new(
            crawl_config.clone(),
            runtime_stats.clone(),
        ));
        let peer_lookup = spawn_peer_lookup(
            options.netmode,
            node_id,
            &sockets_by_bind_addr,
            crawl_engine.snapshot.clone(),
            hash_events_tx.clone(),
            PeerLookupRuntime {
                options: options.peer_lookup.clone(),
                stats: runtime_stats.clone(),
                shutdown: shutdown.clone(),
            },
        );

        let scheduler = MetadataScheduler::new_with_runtime(
            hash_rx,
            fetcher,
            MetadataSchedulerLimits {
                queue_size: options.metadata.max_queue_size,
                concurrency: options.metadata.max_worker_count,
            },
            MetadataSchedulerCallbacks {
                torrent: torrent_callback.clone(),
                fetch_gate: on_metadata_fetch.clone(),
                completion: metadata_completion_callback.clone(),
            },
            metadata_queue_len.clone(),
            shutdown.clone(),
            MetadataSchedulerRuntime {
                stats: runtime_stats.clone(),
                peer_lookup_tx: Some(peer_lookup.request_sender()),
            },
        );
        tokio::spawn(scheduler.run());

        let max_metadata_queue_size = options.metadata.max_queue_size;
        Ok(Self {
            options,
            crawl_config: crawl_config.clone(),
            node_id,
            sockets_by_bind_addr: Arc::new(sockets_by_bind_addr),
            token_secret,
            torrent_callback,
            hash_filter: Arc::new(ArcSwapOption::empty()),
            on_metadata_fetch,
            metadata_completion_callback,
            error_callback: Arc::new(ArcSwapOption::empty()),
            crawl_engine,
            peer_lookup,
            hash_events_tx,
            metadata_queue_len,
            max_metadata_queue_size,
            runtime_stats,
            shutdown,
        })
    }

    /// Registers the asynchronous admission gate invoked before the first real Peer attempt.
    ///
    /// Returning `false` rejects the job without downloading Metadata and without emitting a
    /// [`MetadataFetchCompletion`]. If no gate is registered, jobs are admitted.
    pub fn on_metadata_fetch<F, Fut>(&self, callback: F)
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = bool> + Send + 'static,
    {
        let callback: Arc<MetadataFetchCallback> =
            Arc::new(Box::new(move |hash| Box::pin(callback(hash))));
        self.on_metadata_fetch.store(Some(callback));
    }

    /// Registers a torrent callback whose return is implicitly treated as accepted delivery.
    ///
    /// Registering a new torrent callback replaces the previous one.
    pub fn on_torrent<F>(&self, callback: F)
    where
        F: Fn(TorrentInfo) + Send + Sync + 'static,
    {
        let callback: Arc<TorrentAckCallback> = Arc::new(Box::new(move |torrent| {
            callback(torrent);
            true
        }));
        self.torrent_callback.store(Some(callback));
    }

    /// Registers a torrent callback that acknowledges application delivery.
    ///
    /// Returning `true` produces [`crate::MetadataFetchCompletionStatus::Accepted`]. Returning
    /// `false` produces [`crate::MetadataFetchCompletionStatus::DeliveryRejected`]: the Metadata
    /// download was
    /// valid, but the application did not accept it. A callback panic is caught and treated as
    /// rejected delivery. Registering this callback replaces any previous torrent callback.
    pub fn on_torrent_with_ack<F>(&self, callback: F)
    where
        F: Fn(TorrentInfo) -> bool + Send + Sync + 'static,
    {
        let callback: Arc<TorrentAckCallback> = Arc::new(Box::new(callback));
        self.torrent_callback.store(Some(callback));
    }

    /// Registers a callback invoked exactly once when an admitted Metadata job terminates.
    ///
    /// Gate rejection does not emit a completion. `attempts` counts real Peer network attempts;
    /// failure-cache skips are excluded. Registering a new callback replaces the previous one.
    pub fn on_metadata_fetch_complete<F>(&self, callback: F)
    where
        F: Fn(MetadataFetchCompletion) + Send + Sync + 'static,
    {
        let callback: Arc<MetadataCompletionCallback> = Arc::new(Box::new(callback));
        self.metadata_completion_callback.store(Some(callback));
    }

    /// Registers an early synchronous InfoHash filter for valid `announce_peer` queries.
    ///
    /// Returning `false` prevents the Hash from entering the bounded ingress queue. Registering a
    /// new filter replaces the previous one.
    pub fn filter<F>(&self, filter: F)
    where
        F: Fn(&str) -> bool + Send + Sync + 'static,
    {
        let filter: Arc<FilterCallback> = Arc::new(Box::new(filter));
        self.hash_filter.store(Some(filter));
    }

    /// Alias for [`Self::filter`].
    pub fn set_filter<F>(&self, filter: F)
    where
        F: Fn(&str) -> bool + Send + Sync + 'static,
    {
        self.filter(filter);
    }

    /// Registers a runtime error callback, replacing any previous callback.
    ///
    /// Initialization and `start` errors are still returned through [`Result`].
    pub fn on_error<F>(&self, callback: F)
    where
        F: Fn(crate::error::DHTError) + Send + Sync + 'static,
    {
        let callback: Arc<ErrorCallback> = Arc::new(Box::new(callback));
        self.error_callback.store(Some(callback));
    }

    fn emit_error(&self, error: crate::error::DHTError) {
        if let Some(callback) = self.error_callback.load_full() {
            callback(error);
        }
    }

    /// Returns the current strict FIFO crawl-pool size.
    pub fn get_node_pool_size(&self) -> usize {
        self.crawl_engine.node_count.load(Ordering::Relaxed)
    }

    /// Returns a cheap cloneable handle to transport-neutral atomic runtime statistics.
    pub fn runtime_stats(&self) -> DhtRuntimeStats {
        self.runtime_stats.clone()
    }

    /// Starts crawl and UDP background tasks, then waits until [`Self::shutdown`] is called.
    ///
    /// A shut-down server cannot be restarted. Construct a new [`DHTServer`] for another run.
    pub async fn start(&self) -> Result<()> {
        if self.shutdown.is_cancelled() {
            return Err(crate::error::DHTError::Other(
                "DHT server is already shut down".to_string(),
            ));
        }

        self.crawl_engine.spawn(
            self.options.netmode,
            self.node_id,
            &self.sockets_by_bind_addr,
            self.metadata_queue_len.clone(),
            self.max_metadata_queue_size,
            self.shutdown.clone(),
        );

        let buffer_pool = UdpBufferPool::new();
        let workers = self.spawn_workers(buffer_pool.clone());
        for sock in self.sockets_by_bind_addr.values().cloned() {
            spawn_udp_listener(
                sock,
                workers.clone(),
                self.shutdown.clone(),
                buffer_pool.clone(),
                self.runtime_stats.clone(),
            )?;
        }

        self.shutdown.cancelled().await;
        Ok(())
    }

    /// Cancels UDP, crawl and Metadata tasks. This method is safe to call more than once.
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }

    fn spawn_workers(&self, buffer_pool: UdpBufferPool) -> Vec<WorkerHandle> {
        let server = self.clone();
        let shutdown = self.shutdown.clone();
        let num_workers = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(8);
        let queue_size = 5_000;

        let mut workers: Vec<WorkerHandle> = Vec::with_capacity(num_workers);
        for worker_id in 0..num_workers {
            let (tx, mut rx) = mpsc::channel(queue_size);
            workers.push(tx);

            let server_clone = server.clone();
            let cancellation_token = shutdown.clone();
            let pool = buffer_pool.clone();
            let packet_rate = split_u32_quota(
                self.crawl_config.max_response_rate_per_sec,
                num_workers,
                worker_id,
            );
            let byte_rate = split_u64_quota(
                self.crawl_config.max_response_bytes_per_sec,
                num_workers,
                worker_id,
            );
            let per_source_rate = self.crawl_config.max_response_rate_per_source;

            tokio::spawn(async move {
                let mut response_limiter = WorkerResponseLimiter::new(
                    packet_rate,
                    byte_rate,
                    per_source_rate,
                    Instant::now(),
                );
                let mut response_buffer = BytesMut::with_capacity(512);
                loop {
                    tokio::select! {
                        _ = cancellation_token.cancelled() => break,
                        msg = rx.recv() => {
                            match msg {
                                Some((packet, remote_addr, local_addr)) => {
                                    if let Err(e) = server_clone
                                        .handle_message(
                                            packet.payload(),
                                            remote_addr,
                                            local_addr,
                                            &mut response_limiter,
                                            &mut response_buffer,
                                        )
                                        .await
                                    {
                                        server_clone.emit_error(e);
                                    }
                                    pool.release(packet.buf);
                                }
                                None => break,
                            }
                        }
                    }
                }
            });
        }
        workers
    }

    async fn handle_message(
        &self,
        data: &[u8],
        remote_addr: SocketAddr,
        local_addr: SocketAddr,
        response_limiter: &mut WorkerResponseLimiter,
        response_buffer: &mut BytesMut,
    ) -> Result<()> {
        if self.sockets_by_bind_addr.get(&local_addr).is_none() {
            return Ok(());
        }

        let msg: DhtMessage = match serde_bencode::from_bytes(data) {
            Ok(m) => m,
            Err(_) => {
                #[cfg(feature = "metrics")]
                counter!("dht_messages_parse_error_total").increment(1);
                return Ok(());
            }
        };

        #[cfg(feature = "metrics")]
        {
            let label = match msg.y.as_str() {
                "q" => "q",
                "r" => "r",
                "e" => "e",
                _ => "unknown",
            };
            counter!("dht_messages_processed_total", "type" => label).increment(1);
        }

        match msg.y.as_str() {
            "q" => {
                if let Some(q_type) = &msg.q {
                    self.handle_query(
                        &msg,
                        q_type.as_bytes(),
                        remote_addr,
                        local_addr,
                        response_limiter,
                        response_buffer,
                    )
                    .await?;
                }
            }
            "r" => {
                if let Some(response) = msg.r
                    && let Some(tid) = transaction_id_from_bytes(&msg.t)
                {
                    if is_peer_lookup_tid(&tid) {
                        self.peer_lookup.route_response(remote_addr, tid, response);
                    } else {
                        self.crawl_engine.route_response(remote_addr, tid, response);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn handle_query(
        &self,
        msg: &DhtMessage,
        query_type: &[u8],
        remote_addr: SocketAddr,
        local_addr: SocketAddr,
        response_limiter: &mut WorkerResponseLimiter,
        response_buffer: &mut BytesMut,
    ) -> Result<()> {
        let args = match &msg.a {
            Some(a) => a,
            None => return Ok(()),
        };

        let transaction_id = &msg.t;
        let sender_id: Option<&[u8]> = args.id.as_deref().map(|v| v.as_slice());
        let target_id_fallback: Option<&[u8]> = args
            .target
            .as_deref()
            .or(args.info_hash.as_deref())
            .map(|v| v.as_slice());
        let q_str = std::str::from_utf8(query_type).unwrap_or("");
        self.runtime_stats.inbound_query(q_str);

        if let Some(sender_id) = sender_id
            && sender_id.len() == 20
            && is_valid_node_addr(&remote_addr)
        {
            self.crawl_engine.route_discovered(NodeTuple {
                id: sender_id
                    .try_into()
                    .expect("validated DHT sender id contains 20 bytes"),
                addr: remote_addr,
            });
        }

        #[cfg(feature = "metrics")]
        {
            let label = match q_str {
                "ping" => "ping",
                "find_node" => "find_node",
                "get_peers" => "get_peers",
                "announce_peer" => "announce_peer",
                "vote" => "vote",
                _ => "other_or_invalid",
            };
            counter!("dht_queries_total", "q" => label).increment(1);
        }

        if q_str == "announce_peer" {
            self.handle_announce_peer(args, remote_addr).await?;
        }

        self.send_response(
            QueryResponse {
                transaction_id,
                remote_addr,
                local_addr,
                query_type: q_str,
                sender_id,
                target_id: target_id_fallback,
            },
            response_limiter,
            response_buffer,
        )
        .await?;
        Ok(())
    }

    async fn handle_announce_peer(&self, args: &DhtArgs, addr: SocketAddr) -> Result<()> {
        if let Some(token) = &args.token {
            if !self.validate_token(token, addr) {
                self.runtime_stats.announce_invalid_token();
                #[cfg(feature = "metrics")]
                counter!("dht_announce_peer_blocked_total", "reason" => "invalid_token")
                    .increment(1);
                return Ok(());
            }
        } else {
            self.runtime_stats.announce_invalid_token();
            return Ok(());
        }

        if let Some(info_hash) = &args.info_hash {
            let info_hash_arr: [u8; 20] = match info_hash.as_ref().try_into() {
                Ok(arr) => arr,
                Err(_) => {
                    self.runtime_stats.announce_invalid_token();
                    return Ok(());
                }
            };
            let hash_hex = hex::encode(info_hash_arr);

            if let Some(filter) = self.hash_filter.load_full()
                && !filter(&hash_hex)
            {
                self.runtime_stats.announce_filtered();
                #[cfg(feature = "metrics")]
                counter!("dht_announce_peer_blocked_total", "reason" => "filtered").increment(1);
                return Ok(());
            }

            #[cfg(feature = "metrics")]
            counter!("dht_info_hashes_discovered_total").increment(1);

            let port = if let Some(implied) = args.implied_port {
                if implied != 0 {
                    addr.port()
                } else {
                    args.port.unwrap_or(0)
                }
            } else {
                args.port.unwrap_or(addr.port())
            };

            if port > 0 {
                self.runtime_stats.announce_accepted();
                self.runtime_stats.hash_received();
                let event = HashDiscovered {
                    info_hash: hash_hex,
                    peer_addr: SocketAddr::new(addr.ip(), port),
                    discovered_at: std::time::Instant::now(),
                };

                let enqueue_result = self.hash_events_tx.try_send(event);
                self.runtime_stats.set_hash_ingress_queue_depth(
                    self.hash_events_tx
                        .max_capacity()
                        .saturating_sub(self.hash_events_tx.capacity()),
                );
                if enqueue_result.is_err() {
                    self.runtime_stats.hash_ingress_dropped();
                    #[cfg(feature = "metrics")]
                    counter!("dht_metadata_ingress_dropped_total", "reason" => "queue_full")
                        .increment(1);
                    #[cfg(debug_assertions)]
                    log::debug!("Hash queue is full; dropping hash");
                }
            }
        }
        Ok(())
    }

    async fn send_response(
        &self,
        response: QueryResponse<'_>,
        response_limiter: &mut WorkerResponseLimiter,
        response_buffer: &mut BytesMut,
    ) -> Result<()> {
        let socket = match self.sockets_by_bind_addr.get(&response.local_addr) {
            Some(sock) => sock,
            None => return Ok(()),
        };

        let reference_id = response.sender_id.or(response.target_id);
        let my_id = if let Some(target) = reference_id {
            let generated = neighbor_node_id(target, &self.node_id);
            <[u8; 20]>::try_from(generated.as_slice()).expect("neighbor id is always 20 bytes")
        } else {
            self.node_id
        };
        let token = self.generate_token(response.remote_addr);
        let include_nodes =
            response.query_type == "get_peers" || response.query_type == "find_node";
        let requestor_is_ipv6 = response.remote_addr.is_ipv6();
        let nodes = if include_nodes {
            let filter_ipv6 = match self.options.netmode {
                NetMode::Ipv4Only => Some(false),
                NetMode::Ipv6Only => Some(true),
                NetMode::DualStack => Some(requestor_is_ipv6),
            };
            let snapshot = self.crawl_engine.snapshot.load();
            snapshot.random_nodes(8, filter_ipv6)
        } else {
            Vec::new()
        };
        encode_response(
            response_buffer,
            response.transaction_id,
            &my_id,
            &token,
            &nodes,
            requestor_is_ipv6,
        );
        let is_priority = response.query_type == "ping" || response.query_type == "get_peers";
        match response_limiter.acquire(
            response.remote_addr,
            response_buffer.len(),
            is_priority,
            Instant::now(),
        ) {
            ResponsePermit::Regular => self.runtime_stats.response_normal(),
            ResponsePermit::PriorityReserve => {
                self.runtime_stats.udp_response_priority_reserved();
                #[cfg(feature = "metrics")]
                counter!(
                    "dht_udp_responses_priority_reserved_total",
                    "query" => if response.query_type == "ping" { "ping" } else { "get_peers" }
                )
                .increment(1);
            }
            ResponsePermit::Rejected => {
                self.runtime_stats.udp_response_rate_limited();
                #[cfg(feature = "metrics")]
                counter!("dht_udp_responses_dropped_total", "reason" => "rate_limit").increment(1);
                return Ok(());
            }
        }
        match socket.send_to(response_buffer, response.remote_addr).await {
            Ok(len) => {
                self.runtime_stats.udp_sent(len);
                #[cfg(feature = "metrics")]
                {
                    counter!("dht_udp_bytes_sent_total").increment(len as u64);
                    counter!("dht_udp_packets_sent_total", "type" => "response").increment(1);
                }
            }
            Err(_) => self.runtime_stats.response_send_failed(),
        }
        Ok(())
    }

    fn generate_token(&self, addr: SocketAddr) -> [u8; 8] {
        let mut hasher = ahash::AHasher::default();
        match addr.ip() {
            IpAddr::V4(ip) => ip.octets().hash(&mut hasher),
            IpAddr::V6(ip) => ip.octets().hash(&mut hasher),
        }
        self.token_secret.hash(&mut hasher);
        hasher.finish().to_le_bytes()
    }

    fn validate_token(&self, token: &[u8], addr: SocketAddr) -> bool {
        if token.len() != 8 {
            return false;
        }
        let expected = self.generate_token(addr);
        token == expected
    }
}

#[cfg(test)]
mod response_limiter_tests {
    use super::*;

    #[test]
    fn response_limiter_enforces_packet_byte_and_source_limits() {
        let start = Instant::now();
        let addr: SocketAddr = "8.8.8.8:6881".parse().unwrap();
        let mut limiter = WorkerResponseLimiter::new(2, 200, 1, start);
        assert_eq!(
            limiter.acquire(addr, 100, false, start),
            ResponsePermit::Regular
        );
        assert_eq!(
            limiter.acquire(addr, 100, false, start),
            ResponsePermit::Rejected
        );
        assert_eq!(
            limiter.acquire(addr, 100, false, start + Duration::from_secs(1)),
            ResponsePermit::Regular
        );
        assert_eq!(
            limiter.acquire(
                "1.1.1.1:6881".parse().unwrap(),
                513,
                false,
                start + Duration::from_secs(2)
            ),
            ResponsePermit::Rejected
        );
    }

    #[test]
    fn ping_and_get_peers_can_use_the_priority_reserve() {
        let start = Instant::now();
        let mut limiter = WorkerResponseLimiter::new(10, 1_000, 100, start);
        let first: SocketAddr = "8.8.8.8:6881".parse().unwrap();
        let second: SocketAddr = "1.1.1.1:6881".parse().unwrap();
        let priority: SocketAddr = "9.9.9.9:6881".parse().unwrap();

        assert_eq!(
            limiter.acquire(first, 50, false, start),
            ResponsePermit::Regular
        );
        assert_eq!(
            limiter.acquire(second, 50, false, start),
            ResponsePermit::Regular
        );
        assert_eq!(
            limiter.acquire(priority, 50, false, start),
            ResponsePermit::Rejected
        );
        assert_eq!(
            limiter.acquire(priority, 50, true, start),
            ResponsePermit::PriorityReserve
        );
        assert_eq!(
            limiter.acquire("4.4.4.4:6881".parse().unwrap(), 50, true, start),
            ResponsePermit::Rejected
        );
    }
}
