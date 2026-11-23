mod response_limiter;
use response_limiter::{ResponsePermit, WorkerResponseLimiter};
use crate::addr::is_valid_node_addr;
use crate::crawl_config::ResolvedCrawlConfig;
use crate::crawl_engine::CrawlEngine;
use crate::discovery::{DiscoveryHandle, DiscoveryRuntime, is_sample_tid, spawn_discovery};
use crate::error::Result;
use crate::krpc::{ResponseNodes, encode_response};
use crate::metadata::RbitFetcher;
use crate::node_id::{neighbor_node_id, random_node_id, transaction_id_from_bytes};
use crate::peer_lookup::{
    PeerLookupHandle, PeerLookupRuntime, is_peer_lookup_tid, spawn_peer_lookup,
};
use crate::protocol::{DhtArgs, DhtMessage};
use crate::runtime_stats::{DhtRuntimeLimits, DhtRuntimeStats};
use crate::scheduler::{
    MetadataAdmission, MetadataCompletionCallback, MetadataFetchCallback, MetadataScheduler,
    MetadataSchedulerCallbacks, MetadataSchedulerLimits, MetadataSchedulerRuntime,
    TorrentAckCallback,
};
use crate::types::{DHTOptions, MetadataFetchCompletion, NetMode, NodeTuple, TorrentInfo};
use crate::udp_buffer::UdpBufferPool;
use crate::udp_ingress::{WorkerHandle, spawn_udp_listener};
use arc_swap::ArcSwapOption;
use bytes::BytesMut;
#[cfg(feature = "metrics")]
use metrics::counter;
use rand::Rng;
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(crate) type FilterCallback = Box<dyn Fn(&str) -> bool + Send + Sync + 'static>;
type ErrorCallback = Box<dyn Fn(crate::error::DHTError) + Send + Sync + 'static>;
type InboundPacketCallback = Box<dyn Fn(InboundDhtSample) + Send + Sync + 'static>;

/// Classifies inbound KRPC datagrams for lightweight traffic attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InboundDhtKind {
    Ping,
    FindNode,
    GetPeers,
    AnnouncePeer,
    Response,
    Error,
    Other,
}

impl InboundDhtKind {
    pub(crate) fn from_query_str(q: &str) -> Self {
        match q {
            "ping" => Self::Ping,
            "find_node" => Self::FindNode,
            "get_peers" => Self::GetPeers,
            "announce_peer" => Self::AnnouncePeer,
            _ => Self::Other,
        }
    }
}

/// Zero-allocation stack sample emitted for valid inbound KRPC datagrams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InboundDhtSample {
    /// Source peer IP address.
    pub ip: IpAddr,
    /// Raw UDP datagram payload size in bytes, saturated at `u16::MAX`.
    pub bytes: u16,
    /// Parsed KRPC message category.
    pub kind: InboundDhtKind,
}

struct QueryResponse<'a> {
    transaction_id: &'a [u8],
    remote_addr: SocketAddr,
    local_addr: SocketAddr,
    query_type: &'a str,
    sender_id: Option<&'a [u8]>,
    target_id: Option<&'a [u8]>,
}

#[derive(Debug, Clone)]
/// InfoHash candidate submitted to the Metadata scheduler.
pub struct HashDiscovered {
    /// Lowercase hexadecimal InfoHash.
    pub info_hash: String,
    /// Peer endpoint derived from announce `port`/`implied_port`; absent for hash-only sources.
    pub peer_addr: Option<SocketAddr>,
    /// DHT node that supplied or requested the hash; a seed for active peer lookup.
    pub source_node: Option<NodeTuple>,
    /// Discovery origin, retained through scheduling and yield reporting.
    pub source: HashSource,
    /// Monotonic discovery time used for freshness and queue ordering.
    pub discovered_at: std::time::Instant,
}

/// Origin of an InfoHash candidate. A DHT source is not necessarily a BitTorrent peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashSource {
    Announce,
    GetPeers,
    Sample,
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
    on_inbound_packet: Arc<ArcSwapOption<InboundPacketCallback>>,
    crawl_engine: Arc<CrawlEngine>,
    peer_lookup: PeerLookupHandle,
    discovery: DiscoveryHandle,
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
    if let Err(error) = sock.set_recv_buffer_size(32 * 1024 * 1024) {
        log::warn!("Could not request UDP receive buffer for {addr}: {error}");
    }
    if let Err(error) = sock.set_send_buffer_size(8 * 1024 * 1024) {
        log::warn!("Could not request UDP send buffer for {addr}: {error}");
    }
    log::info!(
        "UDP socket {addr} effective buffers: receive={:?}, send={:?}",
        sock.recv_buffer_size(),
        sock.send_buffer_size()
    );
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
        let fetcher = Arc::new(RbitFetcher::new_with_options(
            options.metadata.timeout_secs,
            options.metadata.connect_timeout_ms,
            options.metadata.peer_failure_cache_capacity,
            options.metadata.peer_failure_ttl_secs,
            runtime_stats.clone(),
        ));
        let torrent_callback = Arc::new(ArcSwapOption::empty());
        let hash_filter = Arc::new(ArcSwapOption::empty());
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
            PeerLookupRuntime {
                options: options.peer_lookup.clone(),
                stats: runtime_stats.clone(),
                shutdown: shutdown.clone(),
            },
        );
        let discovery = spawn_discovery(
            options.netmode,
            node_id,
            &sockets_by_bind_addr,
            crawl_engine.snapshot.clone(),
            DiscoveryRuntime {
                options: options.discovery.clone(),
                stats: runtime_stats.clone(),
                hash_tx: hash_events_tx.clone(),
                filter: hash_filter.clone(),
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
                metadata_options: options.metadata.clone(),
                lookup_timeout: Duration::from_millis(options.peer_lookup.lookup_timeout_ms.max(1)),
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
            hash_filter,
            on_metadata_fetch,
            metadata_completion_callback,
            error_callback: Arc::new(ArcSwapOption::empty()),
            on_inbound_packet: Arc::new(ArcSwapOption::empty()),
            crawl_engine,
            peer_lookup,
            discovery,
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
        self.on_metadata_admission(move |hash| {
            let result = callback(hash);
            async move {
                if result.await {
                    MetadataAdmission::Fetch
                } else {
                    MetadataAdmission::Skip
                }
            }
        });
    }

    /// Registers an admission gate that can defer a candidate while retaining its peers.
    /// Replaces a previously registered boolean or deferred admission callback.
    pub fn on_metadata_admission<F, Fut>(&self, callback: F)
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = MetadataAdmission> + Send + 'static,
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

    /// Registers an early synchronous InfoHash filter for all discovery sources.
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

    /// Registers a synchronous, non-blocking observer invoked for each valid inbound KRPC datagram.
    ///
    /// Callers must avoid blocking or allocating inside `callback` because it runs on the UDP worker hot path.
    /// Registering a new callback replaces any previous observer.
    pub fn on_inbound_dht_packet<F>(&self, callback: F)
    where
        F: Fn(InboundDhtSample) + Send + Sync + 'static,
    {
        let callback: Arc<InboundPacketCallback> = Arc::new(Box::new(callback));
        self.on_inbound_packet.store(Some(callback));
    }

    /// Clears the registered inbound DHT packet observer callback, if any.
    pub fn clear_inbound_dht_packet_observer(&self) {
        self.on_inbound_packet.store(None);
    }

    #[inline]
    fn emit_inbound_packet(&self, ip: IpAddr, payload_len: usize, kind: InboundDhtKind) {
        let guard = self.on_inbound_packet.load();
        if let Some(callback) = guard.as_ref() {
            let bytes = u16::try_from(payload_len).unwrap_or(u16::MAX);
            callback(InboundDhtSample { ip, bytes, kind });
        }
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
                self.runtime_stats.inbound_parse_error();
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
                        data.len(),
                        remote_addr,
                        local_addr,
                        response_limiter,
                        response_buffer,
                    )
                    .await?;
                }
            }
            "r" => {
                self.runtime_stats.inbound_response();
                self.emit_inbound_packet(remote_addr.ip(), data.len(), InboundDhtKind::Response);
                if let Some(response) = msg.r
                    && let Some(tid) = transaction_id_from_bytes(&msg.t)
                {
                    if is_sample_tid(&tid) {
                        self.discovery
                            .route_response(remote_addr, tid, Some(response));
                    } else if is_peer_lookup_tid(&tid) {
                        self.peer_lookup
                            .route_response(remote_addr, tid, Some(response));
                    } else {
                        self.crawl_engine.route_response(remote_addr, tid, response);
                    }
                }
            }
            "e" => {
                self.runtime_stats.inbound_error();
                self.emit_inbound_packet(remote_addr.ip(), data.len(), InboundDhtKind::Error);
                if let Some(tid) = transaction_id_from_bytes(&msg.t) {
                    if is_sample_tid(&tid) {
                        self.discovery.route_response(remote_addr, tid, None);
                    } else if is_peer_lookup_tid(&tid) {
                        self.peer_lookup.route_response(remote_addr, tid, None);
                    }
                }
            }
            _ => {
                self.emit_inbound_packet(remote_addr.ip(), data.len(), InboundDhtKind::Other);
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_query(
        &self,
        msg: &DhtMessage,
        query_type: &[u8],
        payload_len: usize,
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
        let q_str = std::str::from_utf8(query_type).unwrap_or("");
        let target_id_fallback: Option<&[u8]> = if q_str == "get_peers" {
            args.info_hash.as_deref().map(|v| v.as_slice())
        } else {
            args.target
                .as_deref()
                .or(args.info_hash.as_deref())
                .map(|v| v.as_slice())
        };
        self.runtime_stats.inbound_query(q_str);
        self.emit_inbound_packet(
            remote_addr.ip(),
            payload_len,
            InboundDhtKind::from_query_str(q_str),
        );

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
        } else if q_str == "get_peers"
            && is_valid_node_addr(&remote_addr)
            && let Some(hash) = args
                .info_hash
                .as_ref()
                .and_then(|hash| <[u8; 20]>::try_from(hash.as_slice()).ok())
            && let Some(id) = args
                .id
                .as_ref()
                .and_then(|id| <[u8; 20]>::try_from(id.as_slice()).ok())
        {
            self.discovery.observe_get_peers(
                hash,
                NodeTuple {
                    id,
                    addr: remote_addr,
                },
            );
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
                    peer_addr: Some(SocketAddr::new(addr.ip(), port)),
                    source_node: args
                        .id
                        .as_ref()
                        .and_then(|id| <[u8; 20]>::try_from(id.as_slice()).ok())
                        .map(|id| NodeTuple { id, addr }),
                    source: HashSource::Announce,
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

    fn build_response_packet(
        &self,
        response: &QueryResponse<'_>,
        response_buffer: &mut BytesMut,
    ) -> bool {
        let reference_id = if response.query_type == "get_peers" {
            response
                .target_id
                .filter(|id| id.len() == 20)
                .or(response.sender_id)
        } else {
            response.sender_id.or(response.target_id)
        };
        let my_id = if let Some(target) = reference_id {
            let generated = neighbor_node_id(target, &self.node_id);
            <[u8; 20]>::try_from(generated.as_slice()).expect("neighbor id is always 20 bytes")
        } else {
            self.node_id
        };
        let token = self.generate_token(response.remote_addr);
        let requestor_is_ipv6 = response.remote_addr.is_ipv6();
        let snapshot_nodes = if response.query_type == "find_node" {
            let filter_ipv6 = match self.options.netmode {
                NetMode::Ipv4Only => Some(false),
                NetMode::Ipv6Only => Some(true),
                NetMode::DualStack => Some(requestor_is_ipv6),
            };
            let snapshot = self.crawl_engine.snapshot.load();
            Some(snapshot.random_nodes(8, filter_ipv6))
        } else {
            None
        };
        let nodes = if response.query_type == "get_peers" {
            ResponseNodes::Empty
        } else if let Some(ref nodes) = snapshot_nodes {
            ResponseNodes::Slice(nodes)
        } else {
            ResponseNodes::Omit
        };
        encode_response(
            response_buffer,
            response.transaction_id,
            &my_id,
            &token,
            nodes,
            requestor_is_ipv6,
        );
        response.query_type == "ping" || response.query_type == "get_peers"
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

        let is_priority = self.build_response_packet(&response, response_buffer);
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
mod tests {
    use super::*;
    #[tokio::test]
    async fn get_peers_response_forges_info_hash_neighbor_and_empty_nodes() {
        let options = DHTOptions { port: 0, ..DHTOptions::default() };
        let server = DHTServer::new(options).await.unwrap();
        let remote_addr: SocketAddr = "8.8.8.8:6881".parse().unwrap();
        let local_addr = *server.sockets_by_bind_addr.keys().next().unwrap();
        let sender_id = [0x11; 20];
        let info_hash = [0xab; 20];
        let mut buffer = BytesMut::new();

        let is_priority = server.build_response_packet(
            &QueryResponse {
                transaction_id: b"gp",
                remote_addr,
                local_addr,
                query_type: "get_peers",
                sender_id: Some(&sender_id),
                target_id: Some(&info_hash),
            },
            &mut buffer,
        );
        assert!(is_priority);
        let decoded: DhtMessage = serde_bencode::from_bytes(&buffer).unwrap();
        let r = decoded.r.unwrap();
        assert_eq!(&r.id.unwrap()[..6], &info_hash[..6]);
        assert_eq!(r.nodes.unwrap().as_ref(), b"");

        let mut fn_buffer = BytesMut::new();
        let is_fn_priority = server.build_response_packet(
            &QueryResponse {
                transaction_id: b"fn",
                remote_addr,
                local_addr,
                query_type: "find_node",
                sender_id: Some(&sender_id),
                target_id: Some(&[0xcd; 20]),
            },
            &mut fn_buffer,
        );
        assert!(!is_fn_priority);
        let fn_decoded: DhtMessage = serde_bencode::from_bytes(&fn_buffer).unwrap();
        assert_eq!(&fn_decoded.r.unwrap().id.unwrap()[..6], &sender_id[..6]);

        server.shutdown();
    }

    #[tokio::test]
    async fn handle_message_tracks_new_counters_and_inbound_observer() {
        use std::sync::Mutex;

        let options = DHTOptions {
            port: 0,
            ..DHTOptions::default()
        };
        let server = DHTServer::new(options).await.unwrap();
        let local_addr = *server.sockets_by_bind_addr.keys().next().unwrap();
        let mut response_limiter = WorkerResponseLimiter::new(0, 0, 0, Instant::now());
        let mut response_buffer = BytesMut::with_capacity(512);

        let recorded: Arc<Mutex<Vec<InboundDhtSample>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded_cb = recorded.clone();
        server.on_inbound_dht_packet(move |sample| {
            recorded_cb.lock().unwrap().push(sample);
        });

        let peer_v4: SocketAddr = "1.2.3.4:6881".parse().unwrap();
        let peer_v6: SocketAddr = "[2001:4860:4860::8888]:6881".parse().unwrap();

        let ping = b"d1:ad2:id20:aaaaaaaaaaaaaaaaaaaae1:q4:ping1:t2:aa1:y1:qe";
        let find_node =
            b"d1:ad2:id20:aaaaaaaaaaaaaaaaaaaa6:target20:bbbbbbbbbbbbbbbbbbbbe1:q9:find_node1:t2:bb1:y1:qe";
        let get_peers =
            b"d1:ad2:id20:aaaaaaaaaaaaaaaaaaaa9:info_hash20:cccccccccccccccccccce1:q9:get_peers1:t2:cc1:y1:qe";
        let announce_peer = b"d1:ad2:id20:aaaaaaaaaaaaaaaaaaaa9:info_hash20:cccccccccccccccccccc4:porti6881e5:token8:12345678e1:q13:announce_peer1:t2:dd1:y1:qe";
        let response = b"d1:rd2:id20:aaaaaaaaaaaaaaaaaaaae1:t2:ee1:y1:re";
        let error_pkt = b"d1:t2:ff1:y1:ee";
        let invalid = b"not-valid-bencode";

        let packets: &[(&[u8], SocketAddr)] = &[
            (ping, peer_v4),
            (find_node, peer_v4),
            (get_peers, peer_v4),
            (announce_peer, peer_v4),
            (response, peer_v6),
            (error_pkt, peer_v4),
            (invalid, peer_v4),
        ];

        for (payload, remote) in packets {
            server
                .handle_message(
                    payload,
                    *remote,
                    local_addr,
                    &mut response_limiter,
                    &mut response_buffer,
                )
                .await
                .unwrap();
        }

        {
            let guard = recorded.lock().unwrap();
            assert_eq!(guard.len(), 6);
            assert_eq!(
                guard[0],
                InboundDhtSample {
                    ip: peer_v4.ip(),
                    bytes: ping.len() as u16,
                    kind: InboundDhtKind::Ping,
                }
            );
            assert_eq!(
                guard[1],
                InboundDhtSample {
                    ip: peer_v4.ip(),
                    bytes: find_node.len() as u16,
                    kind: InboundDhtKind::FindNode,
                }
            );
            assert_eq!(
                guard[2],
                InboundDhtSample {
                    ip: peer_v4.ip(),
                    bytes: get_peers.len() as u16,
                    kind: InboundDhtKind::GetPeers,
                }
            );
            assert_eq!(
                guard[3],
                InboundDhtSample {
                    ip: peer_v4.ip(),
                    bytes: announce_peer.len() as u16,
                    kind: InboundDhtKind::AnnouncePeer,
                }
            );
            assert_eq!(
                guard[4],
                InboundDhtSample {
                    ip: peer_v6.ip(),
                    bytes: response.len() as u16,
                    kind: InboundDhtKind::Response,
                }
            );
            assert_eq!(
                guard[5],
                InboundDhtSample {
                    ip: peer_v4.ip(),
                    bytes: error_pkt.len() as u16,
                    kind: InboundDhtKind::Error,
                }
            );
        }

        server.clear_inbound_dht_packet_observer();
        server
            .handle_message(
                ping,
                peer_v4,
                local_addr,
                &mut response_limiter,
                &mut response_buffer,
            )
            .await
            .unwrap();
        assert_eq!(recorded.lock().unwrap().len(), 6);

        let obs = server.runtime_stats().observability_snapshot();
        assert_eq!(obs.inbound_ping, 2);
        assert_eq!(obs.inbound_find_node, 1);
        assert_eq!(obs.inbound_get_peers, 1);
        assert_eq!(obs.inbound_announce_peer, 1);
        assert_eq!(obs.inbound_response, 1);
        assert_eq!(obs.inbound_error, 1);
        assert_eq!(obs.inbound_parse_error, 1);

        server.shutdown();
    }
}
