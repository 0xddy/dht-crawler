//! Bounded hash-only discovery. DHT endpoints are lookup seeds, never inferred TCP peers.
use crate::addr::{addr_allowed_by_netmode, is_valid_node_addr};
use crate::budget::RateBucket;
use crate::krpc::{encode_sample_infohashes_query, for_each_response_node};
use crate::node_id::{TransactionId, random_node_id};
use crate::protocol::DhtResponse;
use crate::routing_snapshot::RoutingSnapshot;
use crate::runtime_stats::DhtRuntimeStats;
use crate::server::{FilterCallback, HashDiscovered, HashSource};
use crate::types::{DiscoveryOptions, NetMode, NodeTuple};
use ahash::AHashMap;
use arc_swap::{ArcSwap, ArcSwapOption};
use bytes::BytesMut;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const SAMPLE_TID_TAG: u8 = 0xb5;
const MAX_REMOTE_INTERVAL: u64 = 21_600;
const MAX_LOCAL_INTERVAL: u64 = 7 * 24 * 60 * 60;
const MAX_SAMPLES_PER_RESPONSE: usize = 256;
const MAX_HASH_SOURCES: usize = 4;
const INPUT_CAPACITY: usize = 1_024;
const RESPONSE_CAPACITY: usize = 256;
const TICK: Duration = Duration::from_millis(50);
const SEND_TIMEOUT: Duration = Duration::from_millis(100);
const SEED_REFRESH: Duration = Duration::from_secs(5);
type DueKey = (Instant, u64, SocketAddr);

pub(crate) fn is_sample_tid(tid: &TransactionId) -> bool {
    tid[0] == SAMPLE_TID_TAG
}

struct HashInput {
    hash: [u8; 20],
    node: NodeTuple,
}

struct SampleResponse {
    addr: SocketAddr,
    tid: TransactionId,
    response: Option<DhtResponse>,
}

#[derive(Clone)]
pub(crate) struct DiscoveryHandle {
    input: Option<mpsc::Sender<HashInput>>,
    responses: Option<mpsc::Sender<SampleResponse>>,
    stats: DhtRuntimeStats,
}

impl DiscoveryHandle {
    pub(crate) fn observe_get_peers(&self, hash: [u8; 20], node: NodeTuple) {
        let Some(input) = &self.input else { return };
        self.stats.yield_stats().inc("discovery_get_peers_observed");
        if input.try_send(HashInput { hash, node }).is_err() {
            self.stats.yield_stats().inc("discovery_input_dropped");
        }
    }

    pub(crate) fn route_response(
        &self,
        addr: SocketAddr,
        tid: TransactionId,
        response: Option<DhtResponse>,
    ) {
        if let Some(tx) = &self.responses
            && tx
                .try_send(SampleResponse {
                    addr,
                    tid,
                    response,
                })
                .is_err()
        {
            self.stats.yield_stats().inc("discovery_input_dropped");
        }
    }
}

pub(crate) struct DiscoveryRuntime {
    pub(crate) options: DiscoveryOptions,
    pub(crate) stats: DhtRuntimeStats,
    pub(crate) hash_tx: mpsc::Sender<HashDiscovered>,
    pub(crate) filter: Arc<ArcSwapOption<FilterCallback>>,
    pub(crate) shutdown: CancellationToken,
}

pub(crate) fn spawn_discovery(
    netmode: NetMode,
    local_id: [u8; 20],
    sockets: &HashMap<SocketAddr, Arc<UdpSocket>>,
    snapshot: Arc<ArcSwap<RoutingSnapshot>>,
    runtime: DiscoveryRuntime,
) -> DiscoveryHandle {
    let stats = runtime.stats.clone();
    if !runtime.options.enabled {
        return DiscoveryHandle {
            input: None,
            responses: None,
            stats,
        };
    }
    let (input, input_rx) = mpsc::channel(INPUT_CAPACITY);
    let (responses, response_rx) = mpsc::channel(RESPONSE_CAPACITY);
    let actor = DiscoveryActor::new(netmode, local_id, sockets, snapshot, runtime);
    tokio::spawn(actor.run(input_rx, response_rx));
    DiscoveryHandle {
        input: Some(input),
        responses: Some(responses),
        stats,
    }
}

#[derive(Clone, Copy)]
struct SampleNode {
    node: NodeTuple,
    due: Option<DueKey>,
}

#[derive(Clone, Copy)]
struct PendingSample {
    tid: TransactionId,
    deadline: Instant,
}

struct HashDedup {
    entries: AHashMap<[u8; 20], Vec<(SocketAddr, HashSource)>>,
    expiry: VecDeque<(Instant, [u8; 20])>,
    capacity: usize,
    ttl: Duration,
}

impl HashDedup {
    fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            entries: AHashMap::new(),
            expiry: VecDeque::new(),
            capacity: capacity.max(1),
            ttl,
        }
    }

    fn contains(
        &mut self,
        hash: &[u8; 20],
        addr: SocketAddr,
        source: HashSource,
        now: Instant,
    ) -> bool {
        while self.expiry.front().is_some_and(|(at, _)| *at <= now) {
            let (_, old) = self.expiry.pop_front().unwrap();
            self.entries.remove(&old);
        }
        let Some(sources) = self.entries.get(hash) else {
            return false;
        };
        if let Some((_, previous)) = sources.iter().find(|(old, _)| *old == addr) {
            // A requester can later become a sampler that actually stores this hash.
            return !(*previous == HashSource::GetPeers && source == HashSource::Sample);
        }
        sources.len() >= MAX_HASH_SOURCES
    }

    fn insert(&mut self, hash: [u8; 20], addr: SocketAddr, source: HashSource, now: Instant) {
        if let Some(sources) = self.entries.get_mut(&hash) {
            if let Some((_, previous)) = sources.iter_mut().find(|(old, _)| *old == addr) {
                if source == HashSource::Sample {
                    *previous = source;
                }
            } else if sources.len() < MAX_HASH_SOURCES {
                sources.push((addr, source));
            }
            return; // Source upgrades never slide the hash TTL or append expiry entries.
        }
        if self.entries.len() >= self.capacity
            && let Some((_, old)) = self.expiry.pop_front()
        {
            self.entries.remove(&old);
        }
        let deadline = now + self.ttl;
        self.entries.insert(hash, vec![(addr, source)]);
        self.expiry.push_back((deadline, hash));
    }
}

struct DiscoveryActor {
    options: DiscoveryOptions,
    netmode: NetMode,
    local_id: [u8; 20],
    v4: Option<Arc<UdpSocket>>,
    v6: Option<Arc<UdpSocket>>,
    snapshot: Arc<ArcSwap<RoutingSnapshot>>,
    stats: DhtRuntimeStats,
    hash_tx: mpsc::Sender<HashDiscovered>,
    filter: Arc<ArcSwapOption<FilterCallback>>,
    shutdown: CancellationToken,
    nodes: AHashMap<SocketAddr, SampleNode>,
    due: BTreeSet<DueKey>,
    sequence: u64,
    pending: AHashMap<SocketAddr, PendingSample>,
    cooldowns: AHashMap<SocketAddr, (NodeTuple, Instant)>,
    cooldown_expiry: BTreeSet<(Instant, SocketAddr)>,
    next_tid: u64,
    query_budget: RateBucket,
    get_peers_budget: RateBucket,
    sample_hash_budget: RateBucket,
    dedup: HashDedup,
    next_seed_refresh: Instant,
}

impl DiscoveryActor {
    fn new(
        netmode: NetMode,
        local_id: [u8; 20],
        sockets: &HashMap<SocketAddr, Arc<UdpSocket>>,
        snapshot: Arc<ArcSwap<RoutingSnapshot>>,
        runtime: DiscoveryRuntime,
    ) -> Self {
        let now = Instant::now();
        let mut options = runtime.options;
        // User-provided integer durations must never overflow Instant arithmetic.
        options.sample_query_timeout_secs = options.sample_query_timeout_secs.clamp(1, 300);
        options.sample_min_interval_secs = options.sample_min_interval_secs.min(MAX_LOCAL_INTERVAL);
        options.sample_failure_backoff_secs = options
            .sample_failure_backoff_secs
            .clamp(1, MAX_LOCAL_INTERVAL);
        options.sample_no_new_backoff_secs =
            options.sample_no_new_backoff_secs.min(MAX_LOCAL_INTERVAL);
        options.hash_dedup_ttl_secs = options.hash_dedup_ttl_secs.clamp(1, MAX_LOCAL_INTERVAL);
        Self {
            query_budget: RateBucket::per_second(
                options.sample_queries_per_second,
                options.sample_burst,
                false,
                now,
            ),
            get_peers_budget: RateBucket::per_second(
                options.get_peers_hashes_per_second,
                options.get_peers_burst,
                true,
                now,
            ),
            sample_hash_budget: RateBucket::per_second(
                options.sample_hashes_per_second,
                options.sample_hash_burst,
                true,
                now,
            ),
            dedup: HashDedup::new(
                options.hash_dedup_capacity,
                Duration::from_secs(options.hash_dedup_ttl_secs.max(1)),
            ),
            options,
            netmode,
            local_id,
            v4: sockets
                .iter()
                .find_map(|(addr, socket)| addr.is_ipv4().then(|| socket.clone())),
            v6: sockets
                .iter()
                .find_map(|(addr, socket)| addr.is_ipv6().then(|| socket.clone())),
            snapshot,
            stats: runtime.stats,
            hash_tx: runtime.hash_tx,
            filter: runtime.filter,
            shutdown: runtime.shutdown,
            nodes: AHashMap::new(),
            due: BTreeSet::new(),
            sequence: 0,
            pending: AHashMap::new(),
            cooldowns: AHashMap::new(),
            cooldown_expiry: BTreeSet::new(),
            next_tid: 1,
            next_seed_refresh: now,
        }
    }

    async fn run(
        mut self,
        mut input: mpsc::Receiver<HashInput>,
        mut responses: mpsc::Receiver<SampleResponse>,
    ) {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => break,
                _ = tick.tick() => self.on_tick(Instant::now()).await,
                event = responses.recv() => {
                    let Some(event) = event else { break };
                    self.handle_response(event, Instant::now());
                }
                candidate = input.recv() => {
                    let Some(candidate) = candidate else { break };
                    self.emit_hash(candidate.hash, candidate.node, HashSource::GetPeers, Instant::now());
                }
            }
        }
        self.stats.yield_stats().set("discovery_candidate_nodes", 0);
        self.stats.yield_stats().set("discovery_cooldown_nodes", 0);
        self.stats
            .yield_stats()
            .set("discovery_samples_in_flight", 0);
    }

    fn admit_node(&mut self, node: NodeTuple, now: Instant) {
        if !is_valid_node_addr(&node.addr) || !addr_allowed_by_netmode(&node.addr, self.netmode) {
            return;
        }
        if let Some((_, until)) = self.cooldowns.get(&node.addr).copied() {
            if until > now {
                return; // Re-discovery cannot reset an advertised interval.
            }
            self.cooldowns.remove(&node.addr);
            self.cooldown_expiry.remove(&(until, node.addr));
        }
        if let Some(entry) = self.nodes.get_mut(&node.addr) {
            entry.node = node;
            return;
        }
        if self.nodes.len() >= self.options.sample_max_nodes.max(1) {
            // The active pool can replace queued candidates, but never pending requests.
            let Some(key) = self.due.first().copied().filter(|key| key.0 <= now) else {
                self.stats
                    .yield_stats()
                    .inc("discovery_node_capacity_dropped");
                return;
            };
            self.due.remove(&key);
            self.nodes.remove(&key.2);
        }
        self.nodes.insert(node.addr, SampleNode { node, due: None });
        self.schedule(node.addr, now);
    }

    fn schedule(&mut self, addr: SocketAddr, at: Instant) {
        let Some(entry) = self.nodes.get_mut(&addr) else {
            return;
        };
        if let Some(old) = entry.due.take() {
            self.due.remove(&old);
        }
        self.sequence = self.sequence.wrapping_add(1);
        let key = (at, self.sequence, addr);
        entry.due = Some(key);
        self.due.insert(key);
    }

    fn failure_delay(&self) -> Duration {
        Duration::from_secs(self.options.sample_failure_backoff_secs.max(1))
    }

    fn enter_cooldown(&mut self, addr: SocketAddr, until: Instant) {
        let Some(entry) = self.nodes.remove(&addr) else {
            return;
        };
        if let Some(key) = entry.due {
            self.due.remove(&key);
        }
        // Dispatch reserved this history slot, including for a local send failure.
        debug_assert!(self.cooldowns.len() < self.options.sample_cooldown_capacity.max(1));
        self.cooldowns.insert(addr, (entry.node, until));
        self.cooldown_expiry.insert((until, addr));
    }

    async fn on_tick(&mut self, now: Instant) {
        for _ in 0..256 {
            let Some((until, addr)) = self
                .cooldown_expiry
                .first()
                .copied()
                .filter(|(until, _)| *until <= now)
            else {
                break;
            };
            self.cooldown_expiry.remove(&(until, addr));
            if let Some((node, _)) = self.cooldowns.remove(&addr) {
                self.admit_node(node, now);
            }
        }
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter_map(|(addr, pending)| (pending.deadline <= now).then_some(*addr))
            .collect();
        for addr in expired {
            self.pending.remove(&addr);
            self.stats.yield_stats().inc("discovery_sample_timeouts");
            self.enter_cooldown(addr, now + self.failure_delay());
        }
        if now >= self.next_seed_refresh {
            self.next_seed_refresh = now + SEED_REFRESH;
            let family = match self.netmode {
                NetMode::Ipv4Only => Some(false),
                NetMode::Ipv6Only => Some(true),
                NetMode::DualStack => None,
            };
            let seeds = self.snapshot.load().random_nodes(128, family);
            for node in seeds {
                self.admit_node(node, now);
            }
        }
        // A large configured burst cannot monopolize the single actor iteration.
        for _ in 0..self.options.sample_burst.min(64) {
            if self.pending.len() >= self.options.sample_max_in_flight {
                break;
            }
            let Some(key) = self.due.first().copied().filter(|key| key.0 <= now) else {
                break;
            };
            if self.cooldowns.len().saturating_add(self.pending.len())
                >= self.options.sample_cooldown_capacity.max(1)
            {
                self.stats
                    .yield_stats()
                    .inc("discovery_sample_cooldown_full");
                break;
            }
            if !self.query_budget.try_take_one(now) {
                break;
            }
            self.due.remove(&key);
            let entry = self.nodes.get_mut(&key.2).unwrap();
            entry.due = None;
            let node = entry.node;
            let mut tid = self.next_tid.to_be_bytes();
            tid[0] = SAMPLE_TID_TAG;
            self.next_tid = self.next_tid.wrapping_add(1).max(1);
            let mut buffer = BytesMut::with_capacity(128);
            encode_sample_infohashes_query(&mut buffer, &tid, &random_node_id(), &self.local_id);
            let socket = if node.addr.is_ipv4() {
                &self.v4
            } else {
                &self.v6
            };
            let sent = match socket {
                Some(socket) => matches!(
                    tokio::time::timeout(SEND_TIMEOUT, socket.send_to(&buffer, node.addr)).await,
                    Ok(Ok(_))
                ),
                None => false,
            };
            if sent {
                self.pending.insert(
                    node.addr,
                    PendingSample {
                        tid,
                        deadline: Instant::now()
                            + Duration::from_secs(self.options.sample_query_timeout_secs.max(1)),
                    },
                );
                self.stats.udp_sent(buffer.len());
                self.stats.yield_stats().inc("discovery_sample_queries");
            } else {
                self.stats.yield_stats().inc("discovery_sample_send_failed");
                self.enter_cooldown(node.addr, now + self.failure_delay());
            }
        }
        self.stats
            .yield_stats()
            .set("discovery_candidate_nodes", self.nodes.len() as u64);
        self.stats
            .yield_stats()
            .set("discovery_cooldown_nodes", self.cooldowns.len() as u64);
        self.stats
            .yield_stats()
            .set("discovery_samples_in_flight", self.pending.len() as u64);
    }

    fn handle_response(&mut self, event: SampleResponse, now: Instant) {
        let matched = self
            .pending
            .get(&event.addr)
            .is_some_and(|pending| pending.tid == event.tid && pending.deadline > now);
        if !matched {
            self.stats.yield_stats().inc("discovery_sample_unmatched");
            return;
        }
        self.pending.remove(&event.addr);
        let Some(response) = event.response else {
            self.stats.yield_stats().inc("discovery_sample_unsupported");
            self.enter_cooldown(event.addr, now + self.failure_delay());
            return;
        };
        let (Some(samples), Some(interval), Some(num)) =
            (&response.samples, response.interval, response.num)
        else {
            self.stats.yield_stats().inc("discovery_sample_unsupported");
            self.enter_cooldown(event.addr, now + self.failure_delay());
            return;
        };
        let id = response
            .id
            .as_ref()
            .and_then(|id| <[u8; 20]>::try_from(id.as_slice()).ok());
        if id.is_none()
            || interval > MAX_REMOTE_INTERVAL
            || samples.len() % 20 != 0
            || samples.len() as u64 / 20 > num
        {
            self.stats.yield_stats().inc("discovery_sample_invalid");
            // Honor at least the maximum protocol interval for an out-of-range value.
            self.enter_cooldown(
                event.addr,
                now + self
                    .failure_delay()
                    .max(Duration::from_secs(interval.min(MAX_REMOTE_INTERVAL))),
            );
            return;
        }
        self.stats.yield_stats().inc("discovery_sample_responses");
        let source_node = NodeTuple {
            id: id.unwrap(),
            addr: event.addr,
        };
        self.nodes.get_mut(&event.addr).unwrap().node = source_node;
        let mut novel_hashes = 0;
        for sample in samples.chunks_exact(20).take(MAX_SAMPLES_PER_RESPONSE) {
            let hash = sample.try_into().unwrap();
            if !self
                .dedup
                .contains(&hash, source_node.addr, HashSource::Sample, now)
            {
                novel_hashes += 1;
            }
            self.emit_hash(hash, source_node, HashSource::Sample, now);
        }
        let mut wait_secs = interval.max(self.options.sample_min_interval_secs).max(1);
        // Local queue pressure, filtering or hash-rate limits are not evidence
        // that this remote node has stopped supplying new samples.
        if novel_hashes == 0 && samples.len() / 20 <= MAX_SAMPLES_PER_RESPONSE {
            self.stats.yield_stats().inc("discovery_sample_no_new");
            wait_secs = wait_secs.max(self.options.sample_no_new_backoff_secs);
        }
        self.enter_cooldown(event.addr, now + Duration::from_secs(wait_secs));
        let mut neighbors = Vec::new();
        for_each_response_node(&response, self.netmode, |node| {
            if neighbors.len() < 64 {
                neighbors.push(node);
            }
        });
        for node in neighbors {
            self.admit_node(node, now);
        }
    }

    fn emit_hash(
        &mut self,
        hash: [u8; 20],
        node: NodeTuple,
        source: HashSource,
        now: Instant,
    ) -> bool {
        if self.dedup.contains(&hash, node.addr, source, now) {
            self.stats.yield_stats().inc("discovery_hash_duplicate");
            return false;
        }
        let budget = if source == HashSource::GetPeers {
            &mut self.get_peers_budget
        } else {
            &mut self.sample_hash_budget
        };
        if !budget.try_take_one(now) {
            self.stats
                .yield_stats()
                .inc(if source == HashSource::GetPeers {
                    "discovery_get_peers_rate_limited"
                } else {
                    "discovery_hash_rate_limited"
                });
            return false;
        }
        let info_hash = hex::encode(hash);
        if let Some(filter) = self.filter.load_full()
            && !filter(&info_hash)
        {
            return false;
        }
        let event = HashDiscovered {
            info_hash,
            peer_addr: None,
            source_node: Some(node),
            source,
            discovered_at: now,
        };
        if self.hash_tx.try_send(event).is_err() {
            self.stats.yield_stats().inc("discovery_hash_queue_full");
            return false;
        }
        self.dedup.insert(hash, node.addr, source, now);
        self.stats.hash_received();
        self.stats.set_hash_ingress_queue_depth(
            self.hash_tx
                .max_capacity()
                .saturating_sub(self.hash_tx.capacity()),
        );
        self.stats
            .yield_stats()
            .inc(if source == HashSource::GetPeers {
                "discovery_get_peers_enqueued"
            } else {
                "discovery_sample_hashes_enqueued"
            });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor(options: DiscoveryOptions) -> (DiscoveryActor, mpsc::Receiver<HashDiscovered>) {
        let (hash_tx, rx) = mpsc::channel(128);
        (
            DiscoveryActor::new(
                NetMode::Ipv4Only,
                [1; 20],
                &HashMap::new(),
                Arc::new(ArcSwap::from_pointee(RoutingSnapshot::default())),
                DiscoveryRuntime {
                    options,
                    stats: DhtRuntimeStats::default(),
                    hash_tx,
                    filter: Arc::new(ArcSwapOption::empty()),
                    shutdown: CancellationToken::new(),
                },
            ),
            rx,
        )
    }

    fn node() -> NodeTuple {
        NodeTuple {
            id: [2; 20],
            addr: "8.8.8.8:6881".parse().unwrap(),
        }
    }

    fn expect_response(actor: &mut DiscoveryActor, now: Instant) -> TransactionId {
        expect_node_response(actor, node(), now)
    }

    fn expect_node_response(
        actor: &mut DiscoveryActor,
        target: NodeTuple,
        now: Instant,
    ) -> TransactionId {
        // Test setup can create a new synthetic pending transaction independently
        // of a previous test response; production dispatch always checks history.
        if let Some((_, until)) = actor.cooldowns.remove(&target.addr) {
            actor.cooldown_expiry.remove(&(until, target.addr));
        }
        actor.admit_node(target, now);
        let key = actor
            .nodes
            .get_mut(&target.addr)
            .unwrap()
            .due
            .take()
            .unwrap();
        actor.due.remove(&key);
        let tid = [SAMPLE_TID_TAG; 8];
        actor.pending.insert(
            target.addr,
            PendingSample {
                tid,
                deadline: now + Duration::from_secs(3),
            },
        );
        tid
    }

    fn response(interval: u64, hash: [u8; 20]) -> DhtResponse {
        DhtResponse {
            id: Some(serde_bytes::ByteBuf::from(node().id.to_vec())),
            samples: Some(serde_bytes::ByteBuf::from(hash.to_vec())),
            interval: Some(interval),
            num: Some(1),
            ..Default::default()
        }
    }

    #[test]
    fn sampled_hash_preserves_source_without_inventing_tcp_peer_and_honors_interval() {
        let (mut actor, mut rx) = actor(DiscoveryOptions::default());
        let now = Instant::now();
        let tid = expect_response(&mut actor, now);
        actor.handle_response(
            SampleResponse {
                addr: node().addr,
                tid,
                response: Some(response(21_600, [3; 20])),
            },
            now,
        );
        let candidate = rx.try_recv().unwrap();
        assert_eq!(candidate.peer_addr, None);
        assert_eq!(candidate.source_node, Some(node()));
        assert_eq!(candidate.source, HashSource::Sample);
        assert_eq!(
            actor.cooldowns[&node().addr].1,
            now + Duration::from_secs(21_600)
        );
        actor.admit_node(node(), now + Duration::from_secs(1));
        assert_eq!(
            actor.cooldowns[&node().addr].1,
            now + Duration::from_secs(21_600)
        );
    }

    #[test]
    fn response_requires_both_endpoint_and_tid_and_rejects_malformed_samples() {
        let (mut actor, mut rx) = actor(DiscoveryOptions::default());
        let now = Instant::now();
        let tid = expect_response(&mut actor, now);
        actor.handle_response(
            SampleResponse {
                addr: "1.1.1.1:6881".parse().unwrap(),
                tid,
                response: Some(response(0, [3; 20])),
            },
            now,
        );
        actor.handle_response(
            SampleResponse {
                addr: node().addr,
                tid: [0; 8],
                response: Some(response(0, [3; 20])),
            },
            now,
        );
        assert_eq!(actor.pending.len(), 1);
        assert!(rx.try_recv().is_err());
        let mut invalid = response(0, [3; 20]);
        invalid.samples = Some(serde_bytes::ByteBuf::from(vec![3; 21]));
        actor.handle_response(
            SampleResponse {
                addr: node().addr,
                tid,
                response: Some(invalid),
            },
            now,
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(
            actor.cooldowns[&node().addr].1,
            now + Duration::from_secs(900)
        );
    }

    #[test]
    fn hash_sources_have_separate_budgets_and_shared_bounded_dedup() {
        let (mut actor, mut rx) = actor(DiscoveryOptions {
            get_peers_hashes_per_second: 1,
            get_peers_burst: 1,
            sample_hashes_per_second: 1,
            sample_hash_burst: 1,
            hash_dedup_capacity: 2,
            ..Default::default()
        });
        let now = Instant::now();
        assert!(actor.emit_hash([1; 20], node(), HashSource::GetPeers, now));
        assert!(!actor.emit_hash([2; 20], node(), HashSource::GetPeers, now));
        assert!(actor.emit_hash([2; 20], node(), HashSource::Sample, now));
        assert!(!actor.emit_hash([2; 20], node(), HashSource::Sample, now));
        assert_eq!(rx.try_recv().unwrap().source, HashSource::GetPeers);
        assert_eq!(rx.try_recv().unwrap().source, HashSource::Sample);
        assert!(actor.emit_hash(
            [3; 20],
            node(),
            HashSource::Sample,
            now + Duration::from_secs(1)
        ));
        assert_eq!(actor.dedup.entries.len(), 2);
        assert_eq!(actor.dedup.expiry.len(), 2);
    }

    #[test]
    fn empty_or_duplicate_sampling_is_delayed_and_capacity_cannot_reset_cooldown() {
        let (mut actor, _rx) = actor(DiscoveryOptions {
            sample_max_nodes: 1,
            ..Default::default()
        });
        let now = Instant::now();
        let tid = expect_response(&mut actor, now);
        actor
            .dedup
            .insert([3; 20], node().addr, HashSource::Sample, now);
        actor.handle_response(
            SampleResponse {
                addr: node().addr,
                tid,
                response: Some(response(0, [3; 20])),
            },
            now,
        );
        let next = now + Duration::from_secs(1_800);
        assert_eq!(actor.cooldowns[&node().addr].1, next);
        actor.admit_node(
            NodeTuple {
                id: [4; 20],
                addr: "1.1.1.1:1".parse().unwrap(),
            },
            now,
        );
        assert_eq!(actor.nodes.len(), 1);
        assert_eq!(actor.cooldowns[&node().addr].1, next);
    }

    #[tokio::test]
    async fn timed_out_samples_back_off_and_zero_query_rate_sends_nothing() {
        let (mut actor, _rx) = actor(DiscoveryOptions {
            sample_queries_per_second: 0,
            ..Default::default()
        });
        let now = Instant::now();
        expect_response(&mut actor, now);
        actor.on_tick(now + Duration::from_secs(3)).await;
        assert!(actor.pending.is_empty());
        assert_eq!(
            actor.cooldowns[&node().addr].1,
            now + Duration::from_secs(903)
        );
        actor.on_tick(now + Duration::from_secs(904)).await;
        assert!(actor.pending.is_empty());
        assert!(!actor.cooldowns.contains_key(&node().addr));
        assert!(actor.nodes.contains_key(&node().addr));
    }

    #[test]
    fn unsupported_and_out_of_range_intervals_back_off_without_hashes() {
        let (mut actor, mut rx) = actor(DiscoveryOptions::default());
        let now = Instant::now();
        let tid = expect_response(&mut actor, now);
        actor.handle_response(
            SampleResponse {
                addr: node().addr,
                tid,
                response: None,
            },
            now,
        );
        assert_eq!(
            actor.cooldowns[&node().addr].1,
            now + Duration::from_secs(900)
        );
        let tid = expect_response(&mut actor, now);
        actor.handle_response(
            SampleResponse {
                addr: node().addr,
                tid,
                response: Some(response(21_601, [3; 20])),
            },
            now,
        );
        assert_eq!(
            actor.cooldowns[&node().addr].1,
            now + Duration::from_secs(21_600)
        );
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn sampling_query_rate_and_in_flight_are_bounded_on_real_udp_sockets() {
        let local = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let remote = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (mut actor, _rx) = actor(DiscoveryOptions {
            sample_queries_per_second: 1,
            sample_burst: 1,
            sample_max_in_flight: 1,
            ..Default::default()
        });
        actor.v4 = Some(local);
        let now = Instant::now() + Duration::from_secs(1);
        let addr = remote.local_addr().unwrap();
        // Loopback is inserted directly for this offline transport test; production
        // candidate admission rejects non-public addresses.
        actor.nodes.insert(
            addr,
            SampleNode {
                node: NodeTuple { id: [2; 20], addr },
                due: None,
            },
        );
        actor.schedule(addr, now);
        actor.on_tick(now).await;
        assert_eq!(actor.pending.len(), 1);
        let mut packet = [0u8; 512];
        let (len, _) = tokio::time::timeout(Duration::from_secs(1), remote.recv_from(&mut packet))
            .await
            .unwrap()
            .unwrap();
        let message: crate::protocol::DhtMessage =
            serde_bencode::from_bytes(&packet[..len]).unwrap();
        assert_eq!(message.q.as_deref(), Some("sample_infohashes"));
        assert_eq!(message.t[0], SAMPLE_TID_TAG);
        actor.on_tick(now + Duration::from_secs(1)).await;
        assert_eq!(actor.pending.len(), 1);
        assert!(remote.try_recv_from(&mut packet).is_err());
    }

    #[test]
    fn a_full_output_queue_does_not_poison_discovery_dedup() {
        let (mut actor, _rx) = actor(DiscoveryOptions::default());
        let (tx, mut rx) = mpsc::channel(1);
        actor.hash_tx = tx;
        let now = Instant::now();
        assert!(actor.emit_hash([1; 20], node(), HashSource::GetPeers, now));
        assert!(!actor.emit_hash([2; 20], node(), HashSource::GetPeers, now));
        assert!(
            !actor
                .dedup
                .contains(&[2; 20], node().addr, HashSource::GetPeers, now)
        );
        rx.try_recv().unwrap();
        assert!(actor.emit_hash([2; 20], node(), HashSource::GetPeers, now));
    }

    #[test]
    fn local_backpressure_does_not_penalize_a_node_with_novel_samples() {
        let (mut actor, mut rx) = actor(DiscoveryOptions {
            sample_hashes_per_second: 0,
            ..Default::default()
        });
        let now = Instant::now();
        let tid = expect_response(&mut actor, now);
        actor.handle_response(
            SampleResponse {
                addr: node().addr,
                tid,
                response: Some(response(0, [3; 20])),
            },
            now,
        );
        assert!(rx.try_recv().is_err());
        assert_eq!(
            actor.cooldowns[&node().addr].1,
            now + Duration::from_secs(300)
        );
    }

    #[test]
    fn extreme_configured_durations_are_bounded_before_instant_arithmetic() {
        let (mut actor, _rx) = actor(DiscoveryOptions {
            sample_query_timeout_secs: u64::MAX,
            sample_min_interval_secs: u64::MAX,
            sample_failure_backoff_secs: u64::MAX,
            sample_no_new_backoff_secs: u64::MAX,
            hash_dedup_ttl_secs: u64::MAX,
            ..Default::default()
        });
        let now = Instant::now();
        let tid = expect_response(&mut actor, now);
        actor.handle_response(
            SampleResponse {
                addr: node().addr,
                tid,
                response: Some(response(0, [3; 20])),
            },
            now,
        );
        assert_eq!(
            actor.cooldowns[&node().addr].1,
            now + Duration::from_secs(MAX_LOCAL_INTERVAL)
        );
        assert_eq!(actor.options.sample_query_timeout_secs, 300);
        assert_eq!(actor.dedup.ttl, Duration::from_secs(MAX_LOCAL_INTERVAL));
    }

    #[tokio::test]
    async fn long_remote_cooldowns_do_not_fill_active_candidates_and_stop_exploration() {
        let (mut actor, _rx) = actor(DiscoveryOptions {
            sample_max_nodes: 2,
            sample_cooldown_capacity: 3,
            ..Default::default()
        });
        let now = Instant::now();
        for source in [
            node(),
            NodeTuple {
                id: [4; 20],
                addr: "1.1.1.1:6881".parse().unwrap(),
            },
        ] {
            let tid = expect_node_response(&mut actor, source, now);
            actor.handle_response(
                SampleResponse {
                    addr: source.addr,
                    tid,
                    response: Some(response(21_600, [3; 20])),
                },
                now,
            );
        }
        assert_eq!(actor.cooldowns.len(), 2);
        assert!(actor.nodes.is_empty());
        let local = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let remote = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = remote.local_addr().unwrap();
        actor.v4 = Some(local);
        actor.nodes.insert(
            addr,
            SampleNode {
                node: NodeTuple { id: [5; 20], addr },
                due: None,
            },
        );
        actor.schedule(addr, now);
        actor.on_tick(now + Duration::from_secs(1)).await;
        let mut buffer = [0u8; 512];
        let (len, _) = tokio::time::timeout(Duration::from_secs(1), remote.recv_from(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        let message: crate::protocol::DhtMessage =
            serde_bencode::from_bytes(&buffer[..len]).unwrap();
        assert_eq!(message.q.as_deref(), Some("sample_infohashes"));
        assert_eq!(actor.pending.len(), 1);
        assert_eq!(actor.cooldowns.len(), 2);
    }

    #[tokio::test]
    async fn full_cooldown_history_applies_backpressure_without_forgetting_intervals() {
        let (mut actor, _rx) = actor(DiscoveryOptions {
            sample_max_nodes: 2,
            sample_cooldown_capacity: 1,
            ..Default::default()
        });
        let now = Instant::now();
        let tid = expect_response(&mut actor, now);
        actor.handle_response(
            SampleResponse {
                addr: node().addr,
                tid,
                response: Some(response(21_600, [3; 20])),
            },
            now,
        );
        let other = NodeTuple {
            id: [4; 20],
            addr: "1.1.1.1:6881".parse().unwrap(),
        };
        actor.admit_node(other, now);
        actor.admit_node(node(), now + Duration::from_secs(1));
        actor.on_tick(now + Duration::from_secs(1)).await;
        assert!(actor.pending.is_empty());
        assert!(actor.nodes.contains_key(&other.addr));
        assert!(!actor.nodes.contains_key(&node().addr));
        assert_eq!(
            actor.cooldowns[&node().addr].1,
            now + Duration::from_secs(21_600)
        );
        assert_eq!(actor.cooldown_expiry.len(), 1);
    }

    #[test]
    fn sample_source_upgrades_survive_hash_dedup_with_a_fixed_source_and_expiry_bound() {
        let (mut actor, _rx) = actor(DiscoveryOptions::default());
        let now = Instant::now();
        let hash = [9; 20];
        let sources: Vec<_> = (1..=5)
            .map(|port| NodeTuple {
                id: [port as u8; 20],
                addr: SocketAddr::from(([8, 8, 8, 8], port)),
            })
            .collect();
        assert!(actor.emit_hash(hash, sources[0], HashSource::GetPeers, now));
        assert!(actor.emit_hash(hash, sources[1], HashSource::Sample, now));
        assert!(!actor.emit_hash(hash, sources[1], HashSource::Sample, now));
        assert!(actor.emit_hash(hash, sources[0], HashSource::Sample, now));
        assert!(actor.emit_hash(hash, sources[2], HashSource::Sample, now));
        assert!(actor.emit_hash(hash, sources[3], HashSource::Sample, now));
        assert!(!actor.emit_hash(hash, sources[4], HashSource::Sample, now));
        assert_eq!(actor.dedup.entries[&hash].len(), 4);
        assert_eq!(actor.dedup.expiry.len(), 1);
        assert_eq!(actor.dedup.expiry[0].0, now + Duration::from_secs(900));
        assert!(!actor.emit_hash(
            hash,
            sources[0],
            HashSource::Sample,
            now + Duration::from_secs(899)
        ));
        assert!(actor.emit_hash(
            hash,
            sources[4],
            HashSource::Sample,
            now + Duration::from_secs(900)
        ));
        assert_eq!(actor.dedup.expiry.len(), 1);
    }

    #[test]
    fn queue_rejection_does_not_record_a_new_source_for_an_existing_hash() {
        let (mut actor, _rx) = actor(DiscoveryOptions::default());
        let (tx, mut rx) = mpsc::channel(1);
        actor.hash_tx = tx;
        let now = Instant::now();
        let hash = [9; 20];
        let other = NodeTuple {
            id: [4; 20],
            addr: "1.1.1.1:6881".parse().unwrap(),
        };
        assert!(actor.emit_hash(hash, node(), HashSource::GetPeers, now));
        assert!(!actor.emit_hash(hash, other, HashSource::Sample, now));
        assert_eq!(actor.dedup.entries[&hash].len(), 1);
        rx.try_recv().unwrap();
        assert!(actor.emit_hash(hash, other, HashSource::Sample, now));
        assert_eq!(actor.dedup.entries[&hash].len(), 2);
        assert_eq!(actor.dedup.expiry.len(), 1);
    }

    #[test]
    fn sample_response_neighbors_extend_exploration_without_resetting_sender_cooldown() {
        let (mut actor, _rx) = actor(DiscoveryOptions::default());
        let now = Instant::now();
        let tid = expect_response(&mut actor, now);
        let neighbor = NodeTuple {
            id: [4; 20],
            addr: "1.1.1.1:6881".parse().unwrap(),
        };
        let mut compact = Vec::new();
        for peer in [neighbor, node()] {
            compact.extend_from_slice(&peer.id);
            let std::net::IpAddr::V4(ip) = peer.addr.ip() else {
                unreachable!()
            };
            compact.extend_from_slice(&ip.octets());
            compact.extend_from_slice(&peer.addr.port().to_be_bytes());
        }
        let mut reply = response(21_600, [3; 20]);
        reply.nodes = Some(serde_bytes::ByteBuf::from(compact));
        actor.handle_response(
            SampleResponse {
                addr: node().addr,
                tid,
                response: Some(reply),
            },
            now,
        );
        assert!(actor.nodes.contains_key(&neighbor.addr));
        assert!(!actor.nodes.contains_key(&node().addr));
        assert_eq!(
            actor.cooldowns[&node().addr].1,
            now + Duration::from_secs(21_600)
        );
    }
}
