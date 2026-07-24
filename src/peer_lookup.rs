use crate::budget::RateBucket;
use crate::krpc::{encode_get_peers_query, for_each_response_node, for_each_response_peer};
use crate::node_id::TransactionId;
use crate::protocol::DhtResponse;
use crate::routing_snapshot::{RoutingSnapshot, xor_distance_cmp};
use crate::runtime_stats::DhtRuntimeStats;
use crate::server::HashDiscovered;
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
const MAX_QUERIES_PER_LOOKUP: usize = 12;
const MAX_CONCURRENT_QUERIES_PER_LOOKUP: usize = 4;
const MAX_FRONTIER_NODES: usize = 64;
const MAX_PEERS_PER_LOOKUP: usize = 12;
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);
const QUERY_TIMEOUT: Duration = Duration::from_millis(500);
const MAINTENANCE_INTERVAL: Duration = Duration::from_millis(25);
const REQUEST_CHANNEL_CAPACITY: usize = 1_024;
const RESPONSE_CHANNEL_CAPACITY: usize = 4_096;

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
    info_hash_hex: String,
    frontier: Vec<NodeTuple>,
    seen_nodes: AHashSet<SocketAddr>,
    peers: AHashSet<SocketAddr>,
    queried: usize,
    outstanding: usize,
    deadline: Instant,
}

impl LookupState {
    fn pop_closest(&mut self) -> Option<NodeTuple> {
        let index = self
            .frontier
            .iter()
            .enumerate()
            .min_by(|(_, left), (_, right)| xor_distance_cmp(&left.id, &right.id, &self.info_hash))
            .map(|(index, _)| index)?;
        Some(self.frontier.swap_remove(index))
    }

    fn is_complete(&self, now: Instant) -> bool {
        self.deadline <= now
            || self.peers.len() >= MAX_PEERS_PER_LOOKUP
            || (self.outstanding == 0
                && (self.queried >= MAX_QUERIES_PER_LOOKUP || self.frontier.is_empty()))
    }
}

struct LookupResponse {
    remote_addr: SocketAddr,
    tid: TransactionId,
    response: DhtResponse,
}

#[derive(Clone)]
pub(crate) struct PeerLookupHandle {
    request_tx: mpsc::Sender<[u8; 20]>,
    response_tx: mpsc::Sender<LookupResponse>,
    runtime_stats: DhtRuntimeStats,
}

pub(crate) struct PeerLookupRuntime {
    pub(crate) options: PeerLookupOptions,
    pub(crate) stats: DhtRuntimeStats,
    pub(crate) shutdown: CancellationToken,
}

impl PeerLookupHandle {
    pub(crate) fn request_sender(&self) -> mpsc::Sender<[u8; 20]> {
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
    hash_tx: mpsc::Sender<HashDiscovered>,
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
        hash_tx,
        request_rx,
        response_rx,
        request_budget: RateBucket::per_second(
            options.max_lookups_per_second,
            options.burst,
            true,
            Instant::now(),
        ),
        max_active_lookups: options.max_active_lookups,
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
    hash_tx: mpsc::Sender<HashDiscovered>,
    request_rx: mpsc::Receiver<[u8; 20]>,
    response_rx: mpsc::Receiver<LookupResponse>,
    request_budget: RateBucket,
    max_active_lookups: usize,
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
                    let Some(info_hash) = request else { break };
                    self.start_lookup(info_hash, Instant::now()).await;
                }
            }
        }
    }

    async fn start_lookup(&mut self, info_hash: [u8; 20], now: Instant) {
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
        let frontier =
            self.snapshot
                .load()
                .closest_nodes(&info_hash, MAX_QUERIES_PER_LOOKUP, filter_ipv6);
        if frontier.is_empty() {
            self.runtime_stats.peer_lookup_empty();
            return;
        }

        let lookup_id = self.next_lookup_id;
        self.next_lookup_id = self.next_lookup_id.wrapping_add(1).max(1);
        let seen_nodes = frontier.iter().map(|node| node.addr).collect();
        self.active.insert(
            lookup_id,
            LookupState {
                info_hash,
                info_hash_hex: hex::encode(info_hash),
                frontier,
                seen_nodes,
                peers: AHashSet::new(),
                queried: 0,
                outstanding: 0,
                deadline: now + LOOKUP_TIMEOUT,
            },
        );
        self.runtime_stats.peer_lookup_started();
        #[cfg(feature = "metrics")]
        counter!("dht_peer_lookup_started_total").increment(1);
        self.dispatch_more(lookup_id, now).await;
    }

    async fn dispatch_more(&mut self, lookup_id: u64, now: Instant) {
        loop {
            let Some((node, info_hash)) = self.active.get_mut(&lookup_id).and_then(|state| {
                if state.deadline <= now
                    || state.peers.len() >= MAX_PEERS_PER_LOOKUP
                    || state.queried >= MAX_QUERIES_PER_LOOKUP
                    || state.outstanding >= MAX_CONCURRENT_QUERIES_PER_LOOKUP
                {
                    return None;
                }
                let node = state.pop_closest()?;
                state.queried += 1;
                Some((node, state.info_hash))
            }) else {
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
            let sent = match socket {
                Some(socket) => socket.send_to(&buffer, node.addr).await.is_ok(),
                None => false,
            };
            if !sent {
                self.runtime_stats.peer_lookup_send_failed();
                continue;
            }

            let deadline = now + QUERY_TIMEOUT;
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
        self.finish_if_complete(lookup_id, now);
    }

    async fn handle_response(&mut self, event: LookupResponse, now: Instant) {
        let key = PendingKey {
            addr: event.remote_addr,
            tid: event.tid,
        };
        let Some(pending) = self.pending.remove(&key) else {
            return;
        };
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
        let hash = state.info_hash_hex.clone();
        let lookup_id = pending.lookup_id;
        let _ = state;

        for peer in discovered {
            let event = HashDiscovered {
                info_hash: hash.clone(),
                peer_addr: peer,
                discovered_at: now,
            };
            if self.hash_tx.try_send(event).is_ok() {
                self.runtime_stats.peer_lookup_peer_found();
                #[cfg(feature = "metrics")]
                counter!("dht_peer_lookup_peers_found_total").increment(1);
            } else {
                self.runtime_stats.peer_lookup_output_dropped();
                #[cfg(feature = "metrics")]
                counter!("dht_peer_lookup_dropped_total", "reason" => "hash_queue_full")
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
            .filter_map(|(lookup_id, state)| (state.deadline <= now).then_some(*lookup_id))
            .collect();
        for lookup_id in expired {
            self.finish_lookup(lookup_id);
        }
    }

    fn finish_if_complete(&mut self, lookup_id: u64, now: Instant) {
        if self
            .active
            .get(&lookup_id)
            .is_some_and(|state| state.is_complete(now))
        {
            self.finish_lookup(lookup_id);
        }
    }

    fn finish_lookup(&mut self, lookup_id: u64) {
        self.active.remove(&lookup_id);
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
    async fn lookup_response_feeds_discovered_peer_back_to_metadata_scheduler() {
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
        let (hash_tx, mut hash_rx) = mpsc::channel(4);
        let stats = DhtRuntimeStats::default();
        let shutdown = CancellationToken::new();
        let handle = spawn_peer_lookup(
            NetMode::Ipv4Only,
            [7; 20],
            &sockets,
            snapshot,
            hash_tx,
            PeerLookupRuntime {
                options: PeerLookupOptions::default(),
                stats: stats.clone(),
                shutdown: shutdown.clone(),
            },
        );
        handle.request_tx.send([3; 20]).await.unwrap();

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
                values: Some(vec![serde_bytes::ByteBuf::from(vec![
                    8, 8, 4, 4, 0x1a, 0xe1,
                ])]),
            },
        );

        let event = tokio::time::timeout(Duration::from_secs(1), hash_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.info_hash, hex::encode([3; 20]));
        assert_eq!(event.peer_addr, "8.8.4.4:6881".parse().unwrap());
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.peer_lookup_started, 1);
        assert_eq!(snapshot.peer_lookup_queries, 1);
        assert_eq!(snapshot.peer_lookup_responses, 1);
        assert_eq!(snapshot.peer_lookup_peers_found, 1);
        shutdown.cancel();
    }
}
