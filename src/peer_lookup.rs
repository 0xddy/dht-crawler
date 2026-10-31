use crate::budget::RateBucket;
use crate::krpc::{encode_get_peers_query, for_each_response_node, for_each_response_peer};
use crate::node_id::TransactionId;
use crate::protocol::DhtResponse;
use crate::routing_snapshot::{RoutingSnapshot, xor_distance_cmp};
use crate::runtime_stats::DhtRuntimeStats;
use crate::scheduler::MAX_METADATA_CANDIDATES_PER_HASH;
use crate::types::{NetMode, NodeTuple, PeerLookupOptions};
use ahash::{AHashMap, AHashSet};
use arc_swap::ArcSwap;
use bytes::BytesMut;
#[cfg(feature = "metrics")]
use metrics::counter;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const LOOKUP_TID_TAG: u8 = 0xa5;
const MAX_FRONTIER_NODES: usize = 64;
const MAX_PEERS_PER_LOOKUP: usize = MAX_METADATA_CANDIDATES_PER_HASH;
const MAINTENANCE_INTERVAL: Duration = Duration::from_millis(25);
const REQUEST_CHANNEL_CAPACITY: usize = 1_024;
const RESPONSE_CHANNEL_CAPACITY: usize = 4_096;

fn lookup_duration(millis: u64) -> Duration {
    Duration::from_millis(millis.clamp(1, 300_000))
}

fn lookup_deadline(now: Instant, duration: Duration) -> Instant {
    now.checked_add(duration.min(Duration::from_secs(300)))
        .unwrap_or(now)
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
struct PendingKey {
    addr: SocketAddr,
    tid: TransactionId,
}

#[derive(Debug, Clone, Copy)]
struct PendingQuery {
    lookup_id: u64,
    deadline: Instant,
}

struct LookupState {
    info_hash: [u8; 20],
    peer_tx: mpsc::Sender<SocketAddr>,
    frontier: Vec<NodeTuple>,
    source_nodes: VecDeque<NodeTuple>,
    seen_nodes: AHashSet<SocketAddr>,
    peers: AHashSet<SocketAddr>,
    queried: usize,
    outstanding: usize,
    deadline: Instant,
    max_queries: usize,
}

impl LookupState {
    fn pop_closest(&mut self) -> Option<(NodeTuple, bool)> {
        if let Some(node) = self.source_nodes.pop_front() {
            return Some((node, true));
        }
        let index = self
            .frontier
            .iter()
            .enumerate()
            .min_by(|(_, left), (_, right)| xor_distance_cmp(&left.id, &right.id, &self.info_hash))
            .map(|(index, _)| index)?;
        Some((self.frontier.swap_remove(index), false))
    }

    fn is_complete(&self, now: Instant) -> bool {
        self.finish_reason(now).is_some()
    }

    fn finish_reason(&self, now: Instant) -> Option<&'static str> {
        if self.peer_tx.is_closed() {
            Some("lookup_finished_canceled")
        } else if self.deadline <= now {
            Some("lookup_finished_deadline")
        } else if self.peers.len() >= MAX_PEERS_PER_LOOKUP {
            Some("lookup_finished_peers")
        } else if self.outstanding == 0 && self.queried >= self.max_queries {
            Some("lookup_finished_budget")
        } else if self.outstanding == 0 && self.frontier.is_empty() && self.source_nodes.is_empty()
        {
            Some("lookup_finished_exhausted")
        } else {
            None
        }
    }
}

struct LookupResponse {
    remote_addr: SocketAddr,
    tid: TransactionId,
    response: DhtResponse,
}

#[derive(Clone)]
pub(crate) struct PeerLookupHandle {
    request_tx: mpsc::Sender<PeerLookupRequest>,
    response_tx: mpsc::Sender<LookupResponse>,
    runtime_stats: DhtRuntimeStats,
}

/// The dedicated result stream closes after all lookup peers have been delivered.
/// Dropping its receiver also cancels lookup work on the next maintenance pass.
#[derive(Debug)]
pub(crate) struct PeerLookupRequest {
    pub(crate) info_hash: [u8; 20],
    pub(crate) peer_tx: mpsc::Sender<SocketAddr>,
    /// DHT nodes that supplied the hash. These are lookup seeds, never TCP peers.
    pub(crate) source_nodes: Vec<NodeTuple>,
}

pub(crate) struct PeerLookupRuntime {
    pub(crate) options: PeerLookupOptions,
    pub(crate) stats: DhtRuntimeStats,
    pub(crate) shutdown: CancellationToken,
}

impl PeerLookupHandle {
    pub(crate) fn request_sender(&self) -> mpsc::Sender<PeerLookupRequest> {
        self.request_tx.clone()
    }

    pub(crate) fn route_response(
        &self,
        remote_addr: SocketAddr,
        tid: TransactionId,
        response: DhtResponse,
    ) {
        if self
            .response_tx
            .try_send(LookupResponse {
                remote_addr,
                tid,
                response,
            })
            .is_err()
        {
            self.runtime_stats.peer_lookup_response_dropped();
            #[cfg(feature = "metrics")]
            counter!("dht_peer_lookup_dropped_total", "reason" => "response_queue_full")
                .increment(1);
        }
    }
}

pub(crate) fn is_peer_lookup_tid(tid: &TransactionId) -> bool {
    tid[0] == LOOKUP_TID_TAG
}

pub(crate) fn spawn_peer_lookup(
    netmode: NetMode,
    local_id: [u8; 20],
    sockets: &std::collections::HashMap<SocketAddr, Arc<UdpSocket>>,
    snapshot: Arc<ArcSwap<RoutingSnapshot>>,
    runtime: PeerLookupRuntime,
) -> PeerLookupHandle {
    let PeerLookupRuntime {
        options,
        stats,
        shutdown,
    } = runtime;
    let (request_tx, request_rx) = mpsc::channel(REQUEST_CHANNEL_CAPACITY);
    let (response_tx, response_rx) = mpsc::channel(RESPONSE_CHANNEL_CAPACITY);
    let socket_v4 = sockets
        .iter()
        .find_map(|(addr, socket)| addr.is_ipv4().then(|| socket.clone()));
    let socket_v6 = sockets
        .iter()
        .find_map(|(addr, socket)| addr.is_ipv6().then(|| socket.clone()));
    let actor = PeerLookupActor {
        netmode,
        local_id,
        socket_v4,
        socket_v6,
        snapshot,
        request_rx,
        response_rx,
        request_budget: RateBucket::per_second(
            options.max_lookups_per_second,
            options.burst,
            true,
            Instant::now(),
        ),
        max_active_lookups: options.max_active_lookups,
        max_queries: options.max_queries,
        max_concurrent_queries: options.max_concurrent_queries.max(1),
        lookup_timeout: lookup_duration(options.lookup_timeout_ms),
        query_timeout: lookup_duration(options.query_timeout_ms),
        active: AHashMap::new(),
        pending: AHashMap::new(),
        pending_expiry: VecDeque::new(),
        next_lookup_id: 1,
        next_tid: 1,
        runtime_stats: stats.clone(),
        shutdown,
    };
    tokio::spawn(actor.run());
    PeerLookupHandle {
        request_tx,
        response_tx,
        runtime_stats: stats,
    }
}

struct PeerLookupActor {
    netmode: NetMode,
    local_id: [u8; 20],
    socket_v4: Option<Arc<UdpSocket>>,
    socket_v6: Option<Arc<UdpSocket>>,
    snapshot: Arc<ArcSwap<RoutingSnapshot>>,
    request_rx: mpsc::Receiver<PeerLookupRequest>,
    response_rx: mpsc::Receiver<LookupResponse>,
    request_budget: RateBucket,
    max_active_lookups: usize,
    max_queries: usize,
    max_concurrent_queries: usize,
    lookup_timeout: Duration,
    query_timeout: Duration,
    active: AHashMap<u64, LookupState>,
    pending: AHashMap<PendingKey, PendingQuery>,
    pending_expiry: VecDeque<(Instant, PendingKey)>,
    next_lookup_id: u64,
    next_tid: u64,
    runtime_stats: DhtRuntimeStats,
    shutdown: CancellationToken,
}

impl PeerLookupActor {
    async fn run(mut self) {
        let mut maintenance = tokio::time::interval(MAINTENANCE_INTERVAL);
        maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => break,
                response = self.response_rx.recv() => {
                    let Some(response) = response else { break };
                    self.handle_response(response, Instant::now()).await;
                }
                _ = maintenance.tick() => self.expire(Instant::now()).await,
                request = self.request_rx.recv() => {
                    let Some(request) = request else { break };
                    self.start_lookup(request, Instant::now()).await;
                }
            }
        }
    }

    async fn start_lookup(&mut self, request: PeerLookupRequest, now: Instant) {
        let PeerLookupRequest {
            info_hash,
            peer_tx,
            source_nodes,
        } = request;
        if peer_tx.is_closed() {
            return;
        }
        self.runtime_stats.peer_lookup_requested();
        #[cfg(feature = "metrics")]
        counter!("dht_peer_lookup_requests_total").increment(1);
        if self.active.len() >= self.max_active_lookups || !self.request_budget.try_take_one(now) {
            self.runtime_stats.peer_lookup_rate_limited();
            #[cfg(feature = "metrics")]
            counter!("dht_peer_lookup_dropped_total", "reason" => "rate_limit").increment(1);
            return;
        }

        let filter_ipv6 = match (self.socket_v4.is_some(), self.socket_v6.is_some()) {
            (true, false) => Some(false),
            (false, true) => Some(true),
            _ => None,
        };
        let mut frontier = self.snapshot.load().closest_nodes(
            &info_hash,
            self.max_queries.min(MAX_FRONTIER_NODES),
            filter_ipv6,
        );
        let mut seen_sources = AHashSet::new();
        let source_nodes: VecDeque<_> = source_nodes
            .into_iter()
            .filter(|node| filter_ipv6.is_none_or(|ipv6| node.addr.is_ipv6() == ipv6))
            .filter(|node| seen_sources.insert(node.addr))
            .take(8)
            .collect();
        frontier.retain(|node| !seen_sources.contains(&node.addr));
        if frontier.is_empty() && source_nodes.is_empty() {
            self.runtime_stats.peer_lookup_empty();
            self.runtime_stats
                .yield_stats()
                .inc("lookup_finished_exhausted");
            return;
        }

        let lookup_id = self.next_lookup_id;
        self.next_lookup_id = self.next_lookup_id.wrapping_add(1).max(1);
        let seen_nodes = frontier
            .iter()
            .chain(source_nodes.iter())
            .map(|node| node.addr)
            .collect();
        self.active.insert(
            lookup_id,
            LookupState {
                info_hash,
                peer_tx,
                frontier,
                source_nodes,
                seen_nodes,
                peers: AHashSet::new(),
                queried: 0,
                outstanding: 0,
                deadline: lookup_deadline(now, self.lookup_timeout),
                max_queries: self.max_queries,
            },
        );
        self.runtime_stats.peer_lookup_started();
        #[cfg(feature = "metrics")]
        counter!("dht_peer_lookup_started_total").increment(1);
        self.dispatch_more(lookup_id, now).await;
    }

    async fn dispatch_more(&mut self, lookup_id: u64, _now: Instant) {
        loop {
            let now = Instant::now();
            let Some((node, source_seed, info_hash)) =
                self.active.get_mut(&lookup_id).and_then(|state| {
                    if state.peer_tx.is_closed()
                        || state.deadline <= now
                        || state.peers.len() >= MAX_PEERS_PER_LOOKUP
                        || state.queried >= self.max_queries
                        || state.outstanding >= self.max_concurrent_queries
                    {
                        return None;
                    }
                    let (node, source_seed) = state.pop_closest()?;
                    state.queried += 1;
                    Some((node, source_seed, state.info_hash))
                })
            else {
                break;
            };

            let tid = self.next_transaction_id();
            let key = PendingKey {
                addr: node.addr,
                tid,
            };
            let mut buffer = BytesMut::with_capacity(128);
            encode_get_peers_query(&mut buffer, &tid, &info_hash, &self.local_id);
            let socket = if node.addr.is_ipv4() {
                self.socket_v4.clone()
            } else {
                self.socket_v6.clone()
            };
            let operation_deadline = self.active[&lookup_id].deadline;
            let sent = match socket {
                Some(socket) => tokio::time::timeout_at(
                    tokio::time::Instant::from_std(operation_deadline),
                    socket.send_to(&buffer, node.addr),
                )
                .await
                .is_ok_and(|result| result.is_ok()),
                None => false,
            };
            if !sent {
                self.runtime_stats.peer_lookup_send_failed();
                continue;
            }

            if source_seed {
                self.runtime_stats
                    .yield_stats()
                    .inc("lookup_source_seed_queries");
            }
            let deadline =
                lookup_deadline(Instant::now(), self.query_timeout).min(operation_deadline);
            self.pending.insert(
                key,
                PendingQuery {
                    lookup_id,
                    deadline,
                },
            );
            self.pending_expiry.push_back((deadline, key));
            if let Some(state) = self.active.get_mut(&lookup_id) {
                state.outstanding += 1;
            }
            self.runtime_stats.udp_sent(buffer.len());
            self.runtime_stats.peer_lookup_query();
            #[cfg(feature = "metrics")]
            counter!("dht_peer_lookup_queries_total").increment(1);
        }
        self.finish_if_complete(lookup_id, Instant::now());
    }

    async fn handle_response(&mut self, event: LookupResponse, now: Instant) {
        let key = PendingKey {
            addr: event.remote_addr,
            tid: event.tid,
        };
        let Some(pending) = self.pending.remove(&key) else {
            return;
        };
        if self
            .active
            .get(&pending.lookup_id)
            .is_some_and(|state| state.deadline <= now)
        {
            self.finish_lookup(pending.lookup_id, now);
            return;
        }
        let Some(state) = self.active.get_mut(&pending.lookup_id) else {
            return;
        };
        state.outstanding = state.outstanding.saturating_sub(1);
        self.runtime_stats.peer_lookup_response();

        let mut discovered = Vec::new();
        for_each_response_peer(&event.response, self.netmode, |peer| {
            if state.peers.len() < MAX_PEERS_PER_LOOKUP && state.peers.insert(peer) {
                discovered.push(peer);
            }
        });
        let mut response_nodes = Vec::new();
        for_each_response_node(&event.response, self.netmode, |node| {
            response_nodes.push(node)
        });
        for node in response_nodes {
            if state.frontier.len() >= MAX_FRONTIER_NODES {
                break;
            }
            if state.seen_nodes.insert(node.addr) {
                state.frontier.push(node);
            }
        }
        let peer_tx = state.peer_tx.clone();
        let lookup_id = pending.lookup_id;
        let _ = state;

        for peer in discovered {
            if peer_tx.try_send(peer).is_ok() {
                self.runtime_stats.peer_lookup_peer_found();
                #[cfg(feature = "metrics")]
                counter!("dht_peer_lookup_peers_found_total").increment(1);
            } else if !peer_tx.is_closed() {
                self.runtime_stats.peer_lookup_output_dropped();
                #[cfg(feature = "metrics")]
                counter!("dht_peer_lookup_dropped_total", "reason" => "peer_queue_full")
                    .increment(1);
            }
        }
        self.dispatch_more(lookup_id, now).await;
    }

    async fn expire(&mut self, now: Instant) {
        let mut affected = AHashSet::new();
        while let Some((deadline, key)) = self.pending_expiry.front().copied() {
            if deadline > now {
                break;
            }
            self.pending_expiry.pop_front();
            let should_remove = self
                .pending
                .get(&key)
                .is_some_and(|pending| pending.deadline == deadline);
            if !should_remove {
                continue;
            }
            let pending = self.pending.remove(&key).expect("pending lookup exists");
            if let Some(state) = self.active.get_mut(&pending.lookup_id) {
                state.outstanding = state.outstanding.saturating_sub(1);
                affected.insert(pending.lookup_id);
            }
            self.runtime_stats.peer_lookup_timeout();
        }
        for lookup_id in affected {
            self.dispatch_more(lookup_id, now).await;
        }
        let expired: Vec<_> = self
            .active
            .iter()
            .filter_map(|(lookup_id, state)| state.is_complete(now).then_some(*lookup_id))
            .collect();
        for lookup_id in expired {
            self.finish_lookup(lookup_id, now);
        }
    }

    fn finish_if_complete(&mut self, lookup_id: u64, now: Instant) {
        if self
            .active
            .get(&lookup_id)
            .is_some_and(|state| state.is_complete(now))
        {
            self.finish_lookup(lookup_id, now);
        }
    }

    fn finish_lookup(&mut self, lookup_id: u64, now: Instant) {
        if let Some(state) = self.active.remove(&lookup_id)
            && let Some(reason) = state.finish_reason(now)
        {
            self.runtime_stats.yield_stats().inc(reason);
        }
        self.pending
            .retain(|_, pending| pending.lookup_id != lookup_id);
    }

    fn next_transaction_id(&mut self) -> TransactionId {
        let mut tid = self.next_tid.to_be_bytes();
        tid[0] = LOOKUP_TID_TAG;
        self.next_tid = self.next_tid.wrapping_add(1).max(1);
        tid
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::DhtMessage;

    #[test]
    fn lookup_transaction_ids_have_a_reserved_tag() {
        let tid = [LOOKUP_TID_TAG, 1, 2, 3, 4, 5, 6, 7];
        assert!(is_peer_lookup_tid(&tid));
        assert!(!is_peer_lookup_tid(&[0; 8]));
    }

    #[tokio::test]
    async fn rejected_or_empty_lookup_closes_its_result_stream() {
        for max_active_lookups in [0, 1] {
            let shutdown = CancellationToken::new();
            let stats = DhtRuntimeStats::default();
            let handle = spawn_peer_lookup(
                NetMode::Ipv4Only,
                [7; 20],
                &std::collections::HashMap::new(),
                Arc::new(ArcSwap::from_pointee(RoutingSnapshot::from_nodes(
                    Vec::new(),
                    1,
                ))),
                PeerLookupRuntime {
                    options: PeerLookupOptions {
                        max_active_lookups,
                        ..PeerLookupOptions::default()
                    },
                    stats: stats.clone(),
                    shutdown: shutdown.clone(),
                },
            );
            let (peer_tx, mut peer_rx) = mpsc::channel(12);
            handle
                .request_tx
                .send(PeerLookupRequest {
                    info_hash: [3; 20],
                    peer_tx,
                    source_nodes: Vec::new(),
                })
                .await
                .unwrap();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), peer_rx.recv())
                    .await
                    .unwrap(),
                None
            );
            assert_eq!(stats.snapshot().peer_lookup_started, 0);
            assert_eq!(
                stats.snapshot().peer_lookup_rate_limited,
                u64::from(max_active_lookups == 0)
            );
            shutdown.cancel();
        }
    }

    #[tokio::test]
    async fn lookup_result_stream_delivers_peers_before_closing() {
        let local_socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let remote_socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let remote_addr = remote_socket.local_addr().unwrap();
        let local_addr = local_socket.local_addr().unwrap();
        let sockets = std::collections::HashMap::from([(local_addr, local_socket)]);
        let snapshot = Arc::new(ArcSwap::from_pointee(RoutingSnapshot::from_nodes(
            vec![NodeTuple {
                id: [9; 20],
                addr: remote_addr,
            }],
            1,
        )));
        let (peer_tx, mut peer_rx) = mpsc::channel(MAX_PEERS_PER_LOOKUP);
        let stats = DhtRuntimeStats::default();
        let shutdown = CancellationToken::new();
        let handle = spawn_peer_lookup(
            NetMode::Ipv4Only,
            [7; 20],
            &sockets,
            snapshot,
            PeerLookupRuntime {
                options: PeerLookupOptions::default(),
                stats: stats.clone(),
                shutdown: shutdown.clone(),
            },
        );
        handle
            .request_tx
            .send(PeerLookupRequest {
                info_hash: [3; 20],
                peer_tx,
                source_nodes: Vec::new(),
            })
            .await
            .unwrap();

        let mut buffer = [0u8; 512];
        let (len, source) =
            tokio::time::timeout(Duration::from_secs(1), remote_socket.recv_from(&mut buffer))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(source, local_addr);
        let query: DhtMessage = serde_bencode::from_bytes(&buffer[..len]).unwrap();
        assert_eq!(query.q.as_deref(), Some("get_peers"));
        let tid: TransactionId = query.t.as_ref().try_into().unwrap();
        handle.route_response(
            remote_addr,
            tid,
            DhtResponse {
                id: Some(serde_bytes::ByteBuf::from(vec![9; 20])),
                nodes: None,
                nodes6: None,
                values: Some(
                    (0..MAX_PEERS_PER_LOOKUP + 8)
                        .map(|offset| {
                            let port = (6881 + offset as u16).to_be_bytes();
                            serde_bytes::ByteBuf::from(vec![8, 8, 4, 4, port[0], port[1]])
                        })
                        .collect(),
                ),
                ..DhtResponse::default()
            },
        );

        for offset in 0..MAX_PEERS_PER_LOOKUP {
            let peer = tokio::time::timeout(Duration::from_secs(1), peer_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(peer, SocketAddr::from(([8, 8, 4, 4], 6881 + offset as u16)));
        }
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), peer_rx.recv())
                .await
                .unwrap(),
            None
        );
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.peer_lookup_started, 1);
        assert_eq!(snapshot.peer_lookup_queries, 1);
        assert_eq!(snapshot.peer_lookup_responses, 1);
        assert_eq!(
            snapshot.peer_lookup_peers_found,
            MAX_PEERS_PER_LOOKUP as u64
        );
        shutdown.cancel();
    }

    #[test]
    fn lookup_extreme_timeouts_are_bounded_without_panics() {
        let now = Instant::now();
        assert_eq!(lookup_duration(0), Duration::from_millis(1));
        assert_eq!(lookup_duration(u64::MAX), Duration::from_secs(300));
        assert_eq!(
            lookup_deadline(now, Duration::MAX).duration_since(now),
            Duration::from_secs(300)
        );
    }

    #[tokio::test]
    async fn source_seed_is_sent_before_a_closer_snapshot_node_and_budget_is_exact() {
        let local = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let source = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let fallback = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let source_node = NodeTuple {
            id: [255; 20],
            addr: source.local_addr().unwrap(),
        };
        let fallback_node = NodeTuple {
            id: [3; 20],
            addr: fallback.local_addr().unwrap(),
        };
        let sockets = std::collections::HashMap::from([(local.local_addr().unwrap(), local)]);
        let stats = DhtRuntimeStats::default();
        let shutdown = CancellationToken::new();
        let handle = spawn_peer_lookup(
            NetMode::Ipv4Only,
            [7; 20],
            &sockets,
            Arc::new(ArcSwap::from_pointee(RoutingSnapshot::from_nodes(
                vec![fallback_node],
                1,
            ))),
            PeerLookupRuntime {
                options: PeerLookupOptions {
                    max_queries: 2,
                    max_concurrent_queries: 1,
                    ..PeerLookupOptions::default()
                },
                stats: stats.clone(),
                shutdown: shutdown.clone(),
            },
        );
        let (peer_tx, mut peer_rx) = mpsc::channel(4);
        handle
            .request_tx
            .send(PeerLookupRequest {
                info_hash: [3; 20],
                peer_tx,
                source_nodes: vec![source_node],
            })
            .await
            .unwrap();
        let mut buffer = [0; 512];
        let (len, _) = tokio::time::timeout(Duration::from_secs(1), source.recv_from(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            fallback.try_recv_from(&mut [0; 512]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let message: DhtMessage = serde_bencode::from_bytes(&buffer[..len]).unwrap();
        handle.route_response(
            source_node.addr,
            message.t.as_ref().try_into().unwrap(),
            DhtResponse::default(),
        );
        let (len, _) =
            tokio::time::timeout(Duration::from_secs(1), fallback.recv_from(&mut buffer))
                .await
                .unwrap()
                .unwrap();
        let message: DhtMessage = serde_bencode::from_bytes(&buffer[..len]).unwrap();
        handle.route_response(
            fallback.local_addr().unwrap(),
            message.t.as_ref().try_into().unwrap(),
            DhtResponse::default(),
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(1), peer_rx.recv())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(stats.snapshot().peer_lookup_queries, 2);
        assert_eq!(
            stats.yield_stats().snapshot()["lookup_source_seed_queries"],
            1
        );
        assert_eq!(stats.yield_stats().snapshot()["lookup_finished_budget"], 1);
        shutdown.cancel();
    }

    #[tokio::test]
    async fn lookup_deadline_does_not_dispatch_more_queries_after_a_slow_seed() {
        let local = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let source = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let fallback = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let source_node = NodeTuple {
            id: [255; 20],
            addr: source.local_addr().unwrap(),
        };
        let sockets = std::collections::HashMap::from([(local.local_addr().unwrap(), local)]);
        let stats = DhtRuntimeStats::default();
        let shutdown = CancellationToken::new();
        let handle = spawn_peer_lookup(
            NetMode::Ipv4Only,
            [7; 20],
            &sockets,
            Arc::new(ArcSwap::from_pointee(RoutingSnapshot::from_nodes(
                vec![NodeTuple {
                    id: [3; 20],
                    addr: fallback.local_addr().unwrap(),
                }],
                1,
            ))),
            PeerLookupRuntime {
                options: PeerLookupOptions {
                    max_queries: 12,
                    max_concurrent_queries: 1,
                    lookup_timeout_ms: 30,
                    query_timeout_ms: 500,
                    ..PeerLookupOptions::default()
                },
                stats: stats.clone(),
                shutdown: shutdown.clone(),
            },
        );
        let (peer_tx, mut peer_rx) = mpsc::channel(4);
        handle
            .request_tx
            .send(PeerLookupRequest {
                info_hash: [3; 20],
                peer_tx,
                source_nodes: vec![source_node],
            })
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), peer_rx.recv())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(stats.snapshot().peer_lookup_queries, 1);
        assert_eq!(
            stats.yield_stats().snapshot()["lookup_finished_deadline"],
            1
        );
        assert_eq!(
            fallback.try_recv_from(&mut [0; 512]).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        shutdown.cancel();
    }
}
