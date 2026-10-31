use crate::bootstrap::{BootstrapGate, BootstrapSourcePool, resolve_bootstrap_nodes};
use crate::budget::RateBucket;
use crate::crawl_config::ResolvedCrawlConfig;
use crate::krpc::{for_each_response_node, send_find_node_query};
use crate::node_id::{TransactionId, neighbor_node_id, random_node_id, target_for_prefix};
use crate::node_pool::{AdmissionOutcome, NodePool, ResponsiveReservoir, SubnetKey};
use crate::protocol::DhtResponse;
use crate::routing_snapshot::RoutingSnapshot;
#[cfg(test)]
use crate::runtime_stats::DhtRuntimeLimits;
use crate::runtime_stats::DhtRuntimeStats;
use crate::types::{NetMode, NodeTuple};
use ahash::AHashMap;
use arc_swap::ArcSwap;
use bytes::BytesMut;
#[cfg(feature = "metrics")]
use metrics::{counter, gauge};
use rand::Rng;
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const SCHEDULE_INTERVAL: Duration = Duration::from_millis(5);
const MAX_POOL_SCAN_PER_SCHEDULE: usize = 256;

fn recent_probe_capacity(config: &ResolvedCrawlConfig) -> usize {
    let ttl = config.recent_probe_ttl.as_secs() as u128;
    let query_window = u128::from(config.max_find_node_rate_per_sec)
        .saturating_mul(ttl)
        .saturating_add(u128::from(config.burst));
    let destination_window = u128::from(config.max_new_destinations_per_minute)
        .saturating_mul(ttl)
        .div_ceil(60)
        .saturating_add(u128::from(
            config.max_new_destinations_per_minute.div_ceil(60),
        ));
    // Previously responsive endpoints can keep refreshing their TTL without
    // spending destination tokens. Replies to requests already in flight at the
    // beginning of the window can bring further old endpoints into that set.
    let resident_window = destination_window
        .saturating_add(config.responsive_capacity as u128)
        .saturating_add(config.max_in_flight as u128);
    let bootstrap_enabled = config.bootstrap_max_nodes_per_round > 0 && config.low_watermark > 0;
    // Bootstrap can also refresh old endpoints without destination tokens.
    // Concurrent DNS completions may arrive late, so max_nodes_per_round does
    // not bound the resident set. All such probes still spend query tokens.
    // This is a logical bound only: the map and index allocate as entries arrive.
    let capacity = if bootstrap_enabled {
        query_window
    } else {
        query_window.min(resident_window)
    };
    capacity.max(1).min(usize::MAX as u128) as usize
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
struct PendingKey {
    addr: SocketAddr,
    tid: TransactionId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbePurpose {
    New,
    Revisit,
    Bootstrap,
}

#[derive(Debug, Clone, Copy)]
struct PendingRequest {
    node: NodeTuple,
    purpose: ProbePurpose,
    dispatched_at: Instant,
    deadline: Instant,
    subnet: SubnetKey,
}

enum PriorityEvent {
    Response {
        remote_addr: SocketAddr,
        tid: TransactionId,
        response: DhtResponse,
    },
    SendFailed(PendingKey),
    BootstrapResolved(Vec<SocketAddr>),
}

struct OutboundRequest {
    key: PendingKey,
    node: NodeTuple,
    target: [u8; 20],
    sender_id: [u8; 20],
}

pub(crate) struct CrawlEngine {
    config: ResolvedCrawlConfig,
    priority_tx: mpsc::Sender<PriorityEvent>,
    discovery_tx: mpsc::Sender<NodeTuple>,
    /// One-shot handoff used only by `DHTServer::start`; never touched by the crawl hot path.
    receivers: Mutex<Option<(mpsc::Receiver<PriorityEvent>, mpsc::Receiver<NodeTuple>)>>,
    pub(crate) snapshot: Arc<ArcSwap<RoutingSnapshot>>,
    pub(crate) node_count: Arc<AtomicUsize>,
    runtime_stats: DhtRuntimeStats,
}

impl CrawlEngine {
    pub(crate) fn new(config: ResolvedCrawlConfig, runtime_stats: DhtRuntimeStats) -> Self {
        let (priority_tx, priority_rx) = mpsc::channel(config.priority_event_channel_capacity);
        let (discovery_tx, discovery_rx) = mpsc::channel(config.discovery_event_channel_capacity);
        Self {
            config,
            priority_tx,
            discovery_tx,
            receivers: Mutex::new(Some((priority_rx, discovery_rx))),
            snapshot: Arc::new(ArcSwap::from_pointee(RoutingSnapshot::default())),
            node_count: Arc::new(AtomicUsize::new(0)),
            runtime_stats,
        }
    }

    pub(crate) fn route_discovered(&self, node: NodeTuple) {
        let enqueue_result = self.discovery_tx.try_send(node);
        self.runtime_stats.set_crawl_discovery_queue_depth(
            self.discovery_tx
                .max_capacity()
                .saturating_sub(self.discovery_tx.capacity()),
        );
        if enqueue_result.is_err() {
            self.runtime_stats.crawl_event_dropped_discovered();
            #[cfg(feature = "metrics")]
            counter!("dht_crawl_events_dropped_total", "kind" => "discovered").increment(1);
        }
    }

    pub(crate) fn route_response(
        &self,
        remote_addr: SocketAddr,
        tid: TransactionId,
        response: DhtResponse,
    ) {
        let enqueue_result = self.priority_tx.try_send(PriorityEvent::Response {
            remote_addr,
            tid,
            response,
        });
        self.runtime_stats.set_crawl_priority_queue_depth(
            self.priority_tx
                .max_capacity()
                .saturating_sub(self.priority_tx.capacity()),
        );
        if enqueue_result.is_err() {
            self.runtime_stats.crawl_event_dropped_response();
            #[cfg(feature = "metrics")]
            counter!("dht_crawl_events_dropped_total", "kind" => "response").increment(1);
        }
    }

    pub(crate) fn spawn(
        &self,
        netmode: NetMode,
        local_id: [u8; 20],
        sockets: &HashMap<SocketAddr, Arc<UdpSocket>>,
        metadata_queue_len: Arc<AtomicUsize>,
        max_metadata_queue_size: usize,
        shutdown: CancellationToken,
    ) {
        let Some((priority_rx, discovery_rx)) = self
            .receivers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return;
        };

        let mut egress_v4 = None;
        let mut egress_v6 = None;
        for (bind_addr, socket) in sockets.iter() {
            let (tx, rx) = mpsc::channel(self.config.max_in_flight.max(1));
            spawn_egress(
                socket.clone(),
                rx,
                self.priority_tx.clone(),
                self.runtime_stats.clone(),
                shutdown.clone(),
            );
            if bind_addr.is_ipv4() {
                egress_v4 = Some(tx);
            } else {
                egress_v6 = Some(tx);
            }
        }

        let actor = CrawlActor::new(CrawlActorInit {
            config: self.config.clone(),
            netmode,
            local_id,
            priority_rx,
            discovery_rx,
            priority_tx: self.priority_tx.clone(),
            egress_v4,
            egress_v6,
            snapshot: self.snapshot.clone(),
            node_count: self.node_count.clone(),
            metadata_queue_len,
            max_metadata_queue_size,
            runtime_stats: self.runtime_stats.clone(),
            shutdown,
        });
        tokio::spawn(actor.run());
    }
}

fn spawn_egress(
    socket: Arc<UdpSocket>,
    mut rx: mpsc::Receiver<OutboundRequest>,
    priority_tx: mpsc::Sender<PriorityEvent>,
    runtime_stats: DhtRuntimeStats,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let mut buffer = BytesMut::with_capacity(128);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                request = rx.recv() => {
                    let Some(request) = request else { break };
                    if !send_find_node_query(
                        &request.node.addr,
                        &request.key.tid,
                        &request.target,
                        &request.sender_id,
                        &socket,
                        &mut buffer,
                    ).await {
                        let _ = priority_tx.try_send(PriorityEvent::SendFailed(request.key));
                        runtime_stats.set_crawl_priority_queue_depth(
                            priority_tx
                                .max_capacity()
                                .saturating_sub(priority_tx.capacity()),
                        );
                    } else {
                        runtime_stats.udp_sent(buffer.len());
                    }
                }
            }
        }
    });
}

#[derive(Default)]
struct ActorMetrics {
    admitted: u64,
    replaced: u64,
    duplicate: u64,
    admission_limited: u64,
    invalid: u64,
    queries_new: u64,
    queries_revisit: u64,
    queries_bootstrap: u64,
    responses: u64,
    timeouts: u64,
    send_failures: u64,
    unmatched_responses: u64,
}

impl ActorMetrics {
    fn record_admission(&mut self, outcome: AdmissionOutcome) {
        match outcome {
            AdmissionOutcome::Admitted => self.admitted += 1,
            AdmissionOutcome::Replaced => self.replaced += 1,
            AdmissionOutcome::Duplicate => self.duplicate += 1,
            AdmissionOutcome::RateLimited => self.admission_limited += 1,
            AdmissionOutcome::Invalid => self.invalid += 1,
        }
    }
}

fn record_runtime_admission(stats: &DhtRuntimeStats, outcome: AdmissionOutcome) {
    match outcome {
        AdmissionOutcome::Admitted => stats.node_admitted(),
        AdmissionOutcome::Replaced => stats.node_replaced(),
        AdmissionOutcome::Duplicate => stats.node_dropped_duplicate(),
        AdmissionOutcome::RateLimited => stats.node_dropped_rate_limited(),
        AdmissionOutcome::Invalid => stats.node_dropped_invalid(),
    }
}

struct CrawlActorInit {
    config: ResolvedCrawlConfig,
    netmode: NetMode,
    local_id: [u8; 20],
    priority_rx: mpsc::Receiver<PriorityEvent>,
    discovery_rx: mpsc::Receiver<NodeTuple>,
    priority_tx: mpsc::Sender<PriorityEvent>,
    egress_v4: Option<mpsc::Sender<OutboundRequest>>,
    egress_v6: Option<mpsc::Sender<OutboundRequest>>,
    snapshot: Arc<ArcSwap<RoutingSnapshot>>,
    node_count: Arc<AtomicUsize>,
    metadata_queue_len: Arc<AtomicUsize>,
    max_metadata_queue_size: usize,
    runtime_stats: DhtRuntimeStats,
    shutdown: CancellationToken,
}

struct CrawlActor {
    config: ResolvedCrawlConfig,
    netmode: NetMode,
    local_id: [u8; 20],
    priority_rx: mpsc::Receiver<PriorityEvent>,
    discovery_rx: mpsc::Receiver<NodeTuple>,
    priority_tx: mpsc::Sender<PriorityEvent>,
    egress_v4: Option<mpsc::Sender<OutboundRequest>>,
    egress_v6: Option<mpsc::Sender<OutboundRequest>>,
    pool: NodePool,
    responsive: ResponsiveReservoir,
    pending: AHashMap<PendingKey, PendingRequest>,
    pending_expiry: VecDeque<(Instant, PendingKey)>,
    subnet_in_flight: AHashMap<SubnetKey, usize>,
    query_budget: RateBucket,
    destination_budget: RateBucket,
    bootstrap_gate: BootstrapGate,
    bootstrap_pool: BootstrapSourcePool,
    bootstrap_queue: VecDeque<SocketAddr>,
    snapshot: Arc<ArcSwap<RoutingSnapshot>>,
    node_count: Arc<AtomicUsize>,
    metadata_queue_len: Arc<AtomicUsize>,
    max_metadata_queue_size: usize,
    next_tid: u64,
    metrics: ActorMetrics,
    runtime_stats: DhtRuntimeStats,
    shutdown: CancellationToken,
}

impl CrawlActor {
    fn new(init: CrawlActorInit) -> Self {
        let CrawlActorInit {
            config,
            netmode,
            local_id,
            priority_rx,
            discovery_rx,
            priority_tx,
            egress_v4,
            egress_v6,
            snapshot,
            node_count,
            metadata_queue_len,
            max_metadata_queue_size,
            runtime_stats,
            shutdown,
        } = init;
        let now = Instant::now();
        let destination_burst = config.max_new_destinations_per_minute.div_ceil(60).max(1);
        let recent_capacity = recent_probe_capacity(&config);
        Self {
            pool: NodePool::new(
                config.pool_capacity,
                config.max_replacements_per_minute,
                config.recent_probe_ttl,
                recent_capacity,
                now,
            ),
            responsive: ResponsiveReservoir::new(
                config.responsive_capacity,
                config.responsive_ttl,
                config.revisit_interval,
            ),
            pending: AHashMap::with_capacity(config.max_in_flight),
            pending_expiry: VecDeque::with_capacity(config.max_in_flight),
            subnet_in_flight: AHashMap::new(),
            query_budget: RateBucket::per_second(
                config.max_find_node_rate_per_sec,
                config.burst,
                false,
                now,
            ),
            destination_budget: RateBucket::per_minute(
                config.max_new_destinations_per_minute,
                destination_burst,
                false,
                now,
            ),
            bootstrap_pool: BootstrapSourcePool::new(
                config.bootstrap_nodes.clone(),
                config.bootstrap_backoff_base,
                config.bootstrap_backoff_max,
            ),
            config,
            netmode,
            local_id,
            priority_rx,
            discovery_rx,
            priority_tx,
            egress_v4,
            egress_v6,
            bootstrap_gate: BootstrapGate::new(),
            bootstrap_queue: VecDeque::new(),
            snapshot,
            node_count,
            metadata_queue_len,
            max_metadata_queue_size,
            next_tid: 1,
            metrics: ActorMetrics::default(),
            runtime_stats,
            shutdown,
        }
    }

    async fn run(mut self) {
        let mut schedule_tick = tokio::time::interval(SCHEDULE_INTERVAL);
        schedule_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut snapshot_tick = tokio::time::interval(self.config.snapshot_refresh);
        snapshot_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut metrics_tick = tokio::time::interval(Duration::from_secs(1));
        metrics_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;
                _ = self.shutdown.cancelled() => break,
                _ = schedule_tick.tick() => self.on_schedule_tick(Instant::now()),
                _ = snapshot_tick.tick() => self.publish_snapshot(Instant::now()),
                _ = metrics_tick.tick() => self.flush_metrics(Instant::now()),
                event = self.priority_rx.recv() => {
                    self.runtime_stats
                        .set_crawl_priority_queue_depth(self.priority_rx.len());
                    let Some(event) = event else { break };
                    self.handle_priority(event, Instant::now());
                    self.drain_events();
                }
                node = self.discovery_rx.recv() => {
                    self.runtime_stats
                        .set_crawl_discovery_queue_depth(self.discovery_rx.len());
                    let Some(node) = node else { break };
                    self.admit(node, Instant::now());
                    self.drain_events();
                }
            }
        }
        self.runtime_stats.set_crawl_priority_queue_depth(0);
        self.runtime_stats.set_crawl_discovery_queue_depth(0);
    }

    fn drain_events(&mut self) {
        let mut events = 1;
        let mut nodes = 0;
        while events < self.config.event_batch_limit && nodes < self.config.node_batch_limit {
            if events % 8 == 0
                && let Ok(node) = self.discovery_rx.try_recv()
            {
                self.runtime_stats
                    .set_crawl_discovery_queue_depth(self.discovery_rx.len());
                self.admit(node, Instant::now());
                events += 1;
                nodes += 1;
                continue;
            }
            if let Ok(event) = self.priority_rx.try_recv() {
                self.runtime_stats
                    .set_crawl_priority_queue_depth(self.priority_rx.len());
                self.handle_priority(event, Instant::now());
                events += 1;
                continue;
            }
            if let Ok(node) = self.discovery_rx.try_recv() {
                self.runtime_stats
                    .set_crawl_discovery_queue_depth(self.discovery_rx.len());
                self.admit(node, Instant::now());
                events += 1;
                nodes += 1;
                continue;
            }
            break;
        }
    }

    fn handle_priority(&mut self, event: PriorityEvent, now: Instant) {
        match event {
            PriorityEvent::Response {
                remote_addr,
                tid,
                response,
            } => self.handle_response(remote_addr, tid, response, now),
            PriorityEvent::SendFailed(key) => {
                if let Some(pending) = self.pending.remove(&key) {
                    self.release_pending(pending);
                    self.metrics.send_failures += 1;
                    self.runtime_stats.send_failure();
                    if self
                        .responsive
                        .record_failure(pending.node.addr, pending.dispatched_at, now)
                    {
                        self.runtime_stats.responsive_probe_backoff();
                    }
                    if pending.purpose == ProbePurpose::Bootstrap {
                        self.bootstrap_pool.mark_timeout(pending.node.addr, now);
                    }
                }
            }
            PriorityEvent::BootstrapResolved(candidates) => {
                let selected = self.bootstrap_pool.select(
                    candidates,
                    self.config.bootstrap_max_nodes_per_round,
                    now,
                );
                self.bootstrap_queue.extend(selected);
            }
        }
    }

    fn handle_response(
        &mut self,
        remote_addr: SocketAddr,
        tid: TransactionId,
        response: DhtResponse,
        now: Instant,
    ) {
        let key = PendingKey {
            addr: remote_addr,
            tid,
        };
        let Some(pending) = self.pending.remove(&key) else {
            self.metrics.unmatched_responses += 1;
            self.runtime_stats.unmatched_response();
            return;
        };
        self.release_pending(pending);
        self.metrics.responses += 1;
        self.runtime_stats.response();
        if pending.purpose == ProbePurpose::Bootstrap {
            self.bootstrap_pool.mark_success(remote_addr, now);
        }

        let mut responsive_node = pending.node;
        if let Some(id) = response.id.as_ref()
            && let Ok(id) = <[u8; 20]>::try_from(id.as_slice())
        {
            responsive_node.id = id;
        }
        self.responsive
            .record(responsive_node, now, pending.dispatched_at);

        let pool = &mut self.pool;
        let metrics = &mut self.metrics;
        let runtime_stats = &self.runtime_stats;
        for_each_response_node(&response, self.netmode, |node| {
            let outcome = pool.admit(node, now);
            metrics.record_admission(outcome);
            record_runtime_admission(runtime_stats, outcome);
        });
        self.sync_node_count();
    }

    fn admit(&mut self, node: NodeTuple, now: Instant) {
        let outcome = self.pool.admit(node, now);
        self.metrics.record_admission(outcome);
        record_runtime_admission(&self.runtime_stats, outcome);
        self.sync_node_count();
    }

    fn on_schedule_tick(&mut self, now: Instant) {
        self.expire_pending(now);
        self.maybe_resolve_bootstrap(now);

        let rate = self
            .config
            .rate_for_metadata_pressure(self.metadata_pressure());
        self.runtime_stats.set_find_node_effective_rate(rate);
        self.query_budget.set_per_second_rate(rate, now);
        let budget = self.query_budget.try_take(self.config.burst as usize, now);
        for scheduled in 0..budget {
            if self.pending.len() >= self.config.max_in_flight {
                self.query_budget.refund(budget - scheduled);
                break;
            }
            if !self.schedule_one(now) {
                self.query_budget.refund(budget - scheduled);
                break;
            }
        }
        self.sync_node_count();
    }

    fn schedule_one(&mut self, now: Instant) -> bool {
        if self.pool.len() < self.config.low_watermark
            && let Some(addr) = self.bootstrap_queue.front().copied()
        {
            let is_new = !self.pool.contains_recent(&addr, now);
            if is_new && !self.destination_budget.try_take_one(now) {
                return self.schedule_revisit(now);
            }
            let node = NodeTuple {
                id: self.local_id,
                addr,
            };
            if self.try_dispatch(node, ProbePurpose::Bootstrap, now) {
                self.bootstrap_queue.pop_front();
                self.pool.record_probe(addr, now);
                self.bootstrap_pool.mark_attempt(addr, now);
                self.metrics.queries_bootstrap += 1;
                self.runtime_stats.query_bootstrap();
                return true;
            }
            if is_new {
                self.destination_budget.refund_one();
            }
        }

        let scan_limit = self.pool.len().min(MAX_POOL_SCAN_PER_SCHEDULE);
        for _ in 0..scan_limit {
            let Some(node) = self.pool.front() else {
                break;
            };
            let subnet = SubnetKey::from_addr(&node.addr);
            if self.subnet_count(&subnet) >= self.config.max_in_flight_per_subnet
                || self.responsive.is_backing_off(&node.addr, now)
            {
                self.pool.rotate_front_to_back();
                continue;
            }
            if !self.destination_budget.try_take_one(now) {
                return self.schedule_revisit(now);
            }
            let node = self
                .pool
                .take_front_for_probe(now)
                .expect("front node exists");
            if self.try_dispatch(node, ProbePurpose::New, now) {
                self.metrics.queries_new += 1;
                self.runtime_stats.query_new();
                return true;
            }
            self.pool.restore_front(node, now);
            self.destination_budget.refund_one();
            break;
        }
        self.schedule_revisit(now)
    }

    fn schedule_revisit(&mut self, now: Instant) -> bool {
        let Some(node) = self.responsive.next_revisit(now) else {
            return false;
        };
        let was_recent = self.pool.contains_recent(&node.addr, now);
        if !was_recent && !self.destination_budget.try_take_one(now) {
            self.responsive
                .defer_revisit(node.addr, now + SCHEDULE_INTERVAL, now);
            return false;
        }
        if self.try_dispatch(node, ProbePurpose::Revisit, now) {
            self.pool.record_probe(node.addr, now);
            self.metrics.queries_revisit += 1;
            self.runtime_stats.query_revisit();
            return true;
        }
        if !was_recent {
            self.destination_budget.refund_one();
        }
        self.responsive
            .defer_revisit(node.addr, now + SCHEDULE_INTERVAL, now);
        false
    }

    fn try_dispatch(&mut self, node: NodeTuple, purpose: ProbePurpose, now: Instant) -> bool {
        if self.pending.len() >= self.config.max_in_flight
            || self.responsive.is_backing_off(&node.addr, now)
        {
            return false;
        }
        let subnet = SubnetKey::from_addr(&node.addr);
        if self.subnet_count(&subnet) >= self.config.max_in_flight_per_subnet {
            return false;
        }
        let tx = if node.addr.is_ipv4() {
            self.egress_v4.clone()
        } else {
            self.egress_v6.clone()
        };
        let Some(tx) = tx else {
            return false;
        };
        let Ok(permit) = tx.try_reserve() else {
            return false;
        };

        let tid = self.next_tid.to_be_bytes();
        self.next_tid = self.next_tid.wrapping_add(1).max(1);
        let key = PendingKey {
            addr: node.addr,
            tid,
        };
        let deadline = now + self.config.request_timeout;
        self.pending.insert(
            key,
            PendingRequest {
                node,
                purpose,
                dispatched_at: now,
                deadline,
                subnet,
            },
        );
        self.pending_expiry.push_back((deadline, key));
        *self.subnet_in_flight.entry(subnet).or_insert(0) += 1;
        self.runtime_stats
            .set_find_node_in_flight(self.pending.len());

        let sender_id = if self.config.neighbor_sender_id {
            let generated = neighbor_node_id(&node.id, &self.local_id);
            generated
                .as_slice()
                .try_into()
                .expect("neighbor id is always 20 bytes")
        } else {
            self.local_id
        };
        permit.send(OutboundRequest {
            key,
            node,
            target: self.choose_target(),
            sender_id,
        });
        self.responsive.record_probe(node.addr, now);
        true
    }

    fn choose_target(&self) -> [u8; 20] {
        let total = self
            .config
            .random_walk_percent
            .saturating_add(self.config.sparse_bucket_percent)
            .max(1);
        if rand::thread_rng().gen_range(0..total) < self.config.sparse_bucket_percent {
            // Sparse means underrepresented in our responsive ID keyspace,
            // rather than merely the contacted node's distance from our ID.
            // The prefix list is refreshed with the snapshot, never scanned
            // or rebuilt for an individual outgoing query.
            let prefixes = self.responsive.sparse_prefixes();
            target_for_prefix(prefixes[rand::thread_rng().gen_range(0..prefixes.len())])
        } else {
            random_node_id()
        }
    }

    fn expire_pending(&mut self, now: Instant) {
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
            let pending = self.pending.remove(&key).expect("pending entry exists");
            self.release_pending(pending);
            self.metrics.timeouts += 1;
            self.runtime_stats.timeout();
            if self
                .responsive
                .record_failure(pending.node.addr, pending.dispatched_at, now)
            {
                self.runtime_stats.responsive_probe_backoff();
            }
            if pending.purpose == ProbePurpose::Bootstrap {
                self.bootstrap_pool.mark_timeout(pending.node.addr, now);
            }
        }
    }

    fn release_pending(&mut self, pending: PendingRequest) {
        if let Some(count) = self.subnet_in_flight.get_mut(&pending.subnet) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.subnet_in_flight.remove(&pending.subnet);
            }
        }
        self.runtime_stats
            .set_find_node_in_flight(self.pending.len());
    }

    fn subnet_count(&self, subnet: &SubnetKey) -> usize {
        self.subnet_in_flight.get(subnet).copied().unwrap_or(0)
    }

    fn maybe_resolve_bootstrap(&mut self, now: Instant) {
        if !self.bootstrap_queue.is_empty()
            || !self
                .bootstrap_gate
                .should_bootstrap(self.pool.len(), &self.config, now)
        {
            return;
        }
        let hosts = self.bootstrap_pool.hosts.clone();
        let netmode = self.netmode;
        let priority_tx = self.priority_tx.clone();
        let runtime_stats = self.runtime_stats.clone();
        tokio::spawn(async move {
            let resolved = resolve_bootstrap_nodes(&hosts, netmode).await;
            let _ = priority_tx.try_send(PriorityEvent::BootstrapResolved(resolved));
            runtime_stats.set_crawl_priority_queue_depth(
                priority_tx
                    .max_capacity()
                    .saturating_sub(priority_tx.capacity()),
            );
        });
    }

    fn publish_snapshot(&mut self, now: Instant) {
        let nodes = self
            .responsive
            .snapshot(self.config.routing_snapshot_size, now);
        self.snapshot.store(Arc::new(RoutingSnapshot::from_nodes(
            nodes,
            self.config.routing_snapshot_size,
        )));
    }

    fn metadata_pressure(&self) -> f64 {
        if self.max_metadata_queue_size == 0 {
            1.0
        } else {
            (self.metadata_queue_len.load(Ordering::Relaxed) as f64
                / self.max_metadata_queue_size as f64)
                .min(1.0)
        }
    }

    fn sync_node_count(&self) {
        self.node_count.store(self.pool.len(), Ordering::Relaxed);
        self.runtime_stats.set_node_pool_size(self.pool.len());
    }

    fn flush_metrics(&mut self, now: Instant) {
        let (recent_entries, recent_capacity, expiry_entries) = self.pool.recent_state();
        self.runtime_stats
            .set_crawl_recent_probe_entries(recent_entries, recent_capacity);
        self.runtime_stats
            .set_crawl_recent_expiry_entries(expiry_entries);
        let (expired, expiry_micros, expiry_calls) = self.pool.take_recent_expiry_stats();
        self.runtime_stats
            .record_crawl_recent_expiry_batch(expired, expiry_micros, expiry_calls);
        #[cfg(feature = "metrics")]
        {
            gauge!("dht_crawl_recent_probe_entries").set(recent_entries as f64);
            gauge!("dht_crawl_recent_probe_capacity").set(recent_capacity as f64);
            gauge!("dht_crawl_recent_expiry_entries").set(expiry_entries as f64);
            counter!("dht_crawl_recent_expiry_removed_total").increment(expired as u64);
            counter!("dht_crawl_recent_expiry_micros_total").increment(expiry_micros);
            counter!("dht_crawl_recent_expiry_calls_total").increment(expiry_calls);
            let metadata_pressure = self.metadata_pressure();
            gauge!("dht_node_pool_size").set(self.pool.len() as f64);
            gauge!("dht_node_pool_oldest_age_seconds").set(self.pool.oldest_age(now).as_secs_f64());
            gauge!("dht_find_node_in_flight").set(self.pending.len() as f64);
            gauge!("dht_metadata_queue_pressure_ratio").set(metadata_pressure);
            gauge!("dht_find_node_effective_rate_per_second")
                .set(self.config.rate_for_metadata_pressure(metadata_pressure) as f64);
            counter!("dht_node_pool_admissions_total").increment(self.metrics.admitted);
            counter!("dht_node_pool_replacements_total").increment(self.metrics.replaced);
            counter!("dht_node_pool_dropped_total", "reason" => "duplicate")
                .increment(self.metrics.duplicate);
            counter!("dht_node_pool_dropped_total", "reason" => "rate_limit")
                .increment(self.metrics.admission_limited);
            counter!("dht_node_pool_dropped_total", "reason" => "invalid")
                .increment(self.metrics.invalid);
            counter!("dht_crawl_queries_sent_total", "kind" => "new")
                .increment(self.metrics.queries_new);
            counter!("dht_crawl_queries_sent_total", "kind" => "revisit")
                .increment(self.metrics.queries_revisit);
            counter!("dht_crawl_queries_sent_total", "kind" => "bootstrap")
                .increment(self.metrics.queries_bootstrap);
            counter!("dht_find_node_responses_total").increment(self.metrics.responses);
            counter!("dht_find_node_timeouts_total").increment(self.metrics.timeouts);
            counter!("dht_find_node_send_failures_total").increment(self.metrics.send_failures);
            counter!("dht_find_node_response_unmatched_total")
                .increment(self.metrics.unmatched_responses);
        }
        #[cfg(not(feature = "metrics"))]
        let _ = now;
        self.metrics = ActorMetrics::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CrawlOptions;

    fn node(id: u8, addr: &str) -> NodeTuple {
        NodeTuple {
            id: [id; 20],
            addr: addr.parse().unwrap(),
        }
    }

    fn test_actor(
        config: ResolvedCrawlConfig,
    ) -> (CrawlActor, mpsc::Receiver<OutboundRequest>, DhtRuntimeStats) {
        let (priority_tx, priority_rx) = mpsc::channel(16);
        let (_discovery_tx, discovery_rx) = mpsc::channel(16);
        let (egress_tx, egress_rx) = mpsc::channel(config.max_in_flight);
        let runtime_stats = DhtRuntimeStats::with_limits(DhtRuntimeLimits {
            metadata_queue: 100_000,
            node_pool: config.pool_capacity,
            node_pool_low_watermark: config.low_watermark,
            find_node_in_flight: config.max_in_flight,
            initial_find_node_rate: config.max_find_node_rate_per_sec,
            hash_ingress_queue: 0,
            crawl_priority_queue: config.priority_event_channel_capacity,
            crawl_discovery_queue: config.discovery_event_channel_capacity,
        });
        let actor = CrawlActor::new(CrawlActorInit {
            config,
            netmode: NetMode::Ipv4Only,
            local_id: [7; 20],
            priority_rx,
            discovery_rx,
            priority_tx,
            egress_v4: Some(egress_tx),
            egress_v6: None,
            snapshot: Arc::new(ArcSwap::from_pointee(RoutingSnapshot::default())),
            node_count: Arc::new(AtomicUsize::new(0)),
            metadata_queue_len: Arc::new(AtomicUsize::new(0)),
            max_metadata_queue_size: 100_000,
            runtime_stats: runtime_stats.clone(),
            shutdown: CancellationToken::new(),
        });
        (actor, egress_rx, runtime_stats)
    }

    #[test]
    fn sparse_targets_follow_responsive_prefix_coverage_and_preserve_random_exploration() {
        let mut options = CrawlOptions::default();
        options.target.random_walk_percent = 0;
        options.target.sparse_bucket_percent = 100;
        let (mut actor, _egress_rx, _) = test_actor(ResolvedCrawlConfig::from_options(&options));
        let now = Instant::now();
        for prefix in 0..=u8::MAX {
            if prefix != 83 {
                actor.responsive.record(
                    node(prefix, &format!("8.8.8.8:{}", u16::from(prefix) + 1)),
                    now,
                    now,
                );
            }
        }
        actor.publish_snapshot(now);
        for _ in 0..128 {
            assert_eq!(actor.choose_target()[0], 83);
        }
        actor.config.random_walk_percent = 100;
        actor.config.sparse_bucket_percent = 0;
        let varied = (0..128).any(|_| actor.choose_target()[0] != 83);
        assert!(
            varied,
            "random exploration must not be confined to the sparse prefix"
        );
    }

    #[test]
    fn schedule_tick_refunds_all_unused_tokens_after_partial_dispatch() {
        // Cover both the in-flight limit and running out of eligible nodes.
        for max_in_flight in [1, 16] {
            let mut options = CrawlOptions::default();
            options.rate_limit.max_find_node_rate_per_sec = 10;
            options.rate_limit.burst = 10;
            options.rate_limit.max_in_flight = max_in_flight;
            options.pool.low_watermark = 0;
            let config = ResolvedCrawlConfig::from_options(&options);
            let (mut actor, mut egress_rx, runtime_stats) = test_actor(config);
            let now = Instant::now() + Duration::from_secs(1);
            actor.pool.admit(node(1, "8.8.8.8:6881"), now);

            actor.on_schedule_tick(now);

            assert!(egress_rx.try_recv().is_ok());
            assert_eq!(runtime_stats.snapshot().queries_new, 1);
            assert_eq!(actor.query_budget.try_take(10, now), 9);
        }
    }

    #[test]
    fn schedule_tick_refunds_the_entire_batch_when_no_node_is_eligible() {
        let mut options = CrawlOptions::default();
        options.rate_limit.max_find_node_rate_per_sec = 10;
        options.rate_limit.burst = 10;
        options.pool.low_watermark = 0;
        let (mut actor, _egress_rx, _) = test_actor(ResolvedCrawlConfig::from_options(&options));
        let now = Instant::now() + Duration::from_secs(1);

        actor.on_schedule_tick(now);

        assert_eq!(actor.query_budget.try_take(10, now), 10);
    }

    #[test]
    fn failed_revisit_dispatch_retries_without_consuming_the_cooldown() {
        let mut options = CrawlOptions::default();
        options.pool.revisit_interval_secs = 30;
        let (mut actor, egress_rx, runtime_stats) =
            test_actor(ResolvedCrawlConfig::from_options(&options));
        let start = Instant::now();
        let now = start + Duration::from_secs(30);
        let first = node(1, "8.8.8.8:6881");
        actor.responsive.record(first, start, start);
        actor.pool.record_probe(first.addr, start);
        drop(egress_rx);

        assert!(!actor.schedule_revisit(now));
        let (egress_tx, mut egress_rx) = mpsc::channel(1);
        actor.egress_v4 = Some(egress_tx);
        assert!(!actor.schedule_revisit(now));
        assert!(actor.schedule_revisit(now + SCHEDULE_INTERVAL));
        assert_eq!(egress_rx.try_recv().unwrap().node, first);
        assert_eq!(runtime_stats.snapshot().queries_revisit, 1);
        assert!(!actor.schedule_revisit(now + Duration::from_secs(29)));
    }

    #[test]
    fn send_failures_and_timeouts_back_off_without_blocking_healthy_pool_nodes() {
        for send_failed in [true, false] {
            let mut options = CrawlOptions::default();
            options.pool.low_watermark = 0;
            let config = ResolvedCrawlConfig::from_options(&options);
            let (mut actor, mut egress_rx, runtime_stats) = test_actor(config);
            let start = Instant::now() + Duration::from_secs(1);
            let blocked = node(1, "8.8.8.8:6881");
            let healthy = node(2, "1.1.1.1:6881");
            actor.responsive.record(blocked, start, start);
            actor.pool.admit(blocked, start);
            actor.pool.admit(healthy, start);
            assert!(actor.try_dispatch(
                blocked,
                ProbePurpose::Revisit,
                start + Duration::from_secs(1)
            ));
            let request = egress_rx.try_recv().unwrap();
            let now = start + Duration::from_secs(4);
            if send_failed {
                actor.handle_priority(PriorityEvent::SendFailed(request.key), now);
            } else {
                actor.expire_pending(now);
            }
            assert!(actor.responsive.is_backing_off(&blocked.addr, now));
            assert!(!actor.try_dispatch(blocked, ProbePurpose::Bootstrap, now));
            assert!(actor.responsive.snapshot(10, now).is_empty());
            assert!(actor.schedule_one(now));
            assert_eq!(egress_rx.try_recv().unwrap().node, healthy);
            assert_eq!(actor.pool.front(), Some(blocked));
            assert_eq!(
                runtime_stats
                    .observability_snapshot()
                    .responsive_probe_backoffs,
                1
            );
            assert!(actor.schedule_one(now + Duration::from_secs(5)));
            assert_eq!(egress_rx.try_recv().unwrap().node, blocked);
        }
    }

    #[test]
    fn recent_cache_limit_covers_resident_refreshes_and_bootstrap_probes() {
        let mut options = CrawlOptions::default();
        options.rate_limit.max_find_node_rate_per_sec = 300_000;
        options.rate_limit.burst = 3_840;
        options.rate_limit.max_in_flight = 98_304;
        options.rate_limit.max_new_destinations_per_minute = 1_440_000;
        options.pool.responsive_capacity = 589_824;
        options.pool.recent_probe_ttl_secs = 180;
        let with_bootstrap = ResolvedCrawlConfig::from_options(&options);
        assert_eq!(recent_probe_capacity(&with_bootstrap), 54_003_840);
        options.pool.low_watermark = 0;
        let (mut actor, _egress_rx, stats) =
            test_actor(ResolvedCrawlConfig::from_options(&options));
        assert_eq!(actor.pool.recent_state(), (0, 5_032_128, 0));
        let now = Instant::now();
        actor.pool.record_probe(node(1, "8.8.8.8:1").addr, now);
        actor.flush_metrics(now);
        let snapshot = stats.observability_snapshot();
        assert_eq!(snapshot.crawl_recent_probe_entries, 1);
        assert_eq!(snapshot.crawl_recent_probe_capacity, 5_032_128);
        assert_eq!(snapshot.crawl_recent_expiry_entries, 1);
    }

    #[test]
    fn refreshing_old_responsive_nodes_does_not_evict_the_new_destination_window() {
        let mut options = CrawlOptions::default();
        options.rate_limit.max_find_node_rate_per_sec = 100;
        options.rate_limit.burst = 10;
        options.rate_limit.max_in_flight = 2;
        options.rate_limit.max_new_destinations_per_minute = 60;
        options.pool.responsive_capacity = 3;
        options.pool.recent_probe_ttl_secs = 10;
        options.pool.low_watermark = 0;
        let (mut actor, _egress_rx, _) = test_actor(ResolvedCrawlConfig::from_options(&options));
        assert_eq!(actor.pool.recent_state().1, 11 + 3 + 2);
        let start = Instant::now();
        let old: Vec<_> = (1..=3)
            .map(|port| SocketAddr::from(([8, 8, 8, 8], port)))
            .collect();
        for addr in &old {
            actor.pool.record_probe(*addr, start);
        }
        // One new endpoint/second is within the destination budget, while the
        // already-responsive endpoints refresh without spending that budget.
        for second in 1..=10 {
            let now = start + Duration::from_secs(second);
            for addr in &old {
                actor.pool.record_probe(*addr, now);
            }
            actor
                .pool
                .record_probe(SocketAddr::from(([1, 1, 1, 1], second as u16)), now);
        }
        let now = start + Duration::from_secs(10);
        for addr in old {
            assert!(actor.pool.contains_recent(&addr, now));
        }
        for port in 1..=10 {
            assert!(
                actor
                    .pool
                    .contains_recent(&SocketAddr::from(([1, 1, 1, 1], port)), now)
            );
        }
        assert_eq!(actor.pool.recent_state(), (13, 16, 13));
    }

    #[test]
    fn zero_rates_are_finite_and_zero_destination_budget_does_not_disable_the_limit() {
        let mut options = CrawlOptions::default();
        options.pool.low_watermark = 0;
        options.rate_limit.max_new_destinations_per_minute = 0;
        let config = ResolvedCrawlConfig::from_options(&options);
        let expected = config.responsive_capacity + config.max_in_flight;
        let query_bound = config.max_find_node_rate_per_sec as usize
            * config.recent_probe_ttl.as_secs() as usize
            + config.burst as usize;
        assert_eq!(
            recent_probe_capacity(&config),
            expected.min(query_bound).max(1)
        );
        let (mut actor, _egress_rx, _) = test_actor(config);
        assert!(
            !actor
                .destination_budget
                .try_take_one(Instant::now() + Duration::from_secs(1000))
        );
        options.rate_limit.max_find_node_rate_per_sec = 0;
        let config = ResolvedCrawlConfig::from_options(&options);
        assert_eq!(recent_probe_capacity(&config), 1);
    }

    #[test]
    fn saturated_head_subnet_does_not_block_later_node() {
        let config = ResolvedCrawlConfig::from_options(&CrawlOptions::default());
        let max_per_subnet = config.max_in_flight_per_subnet;
        let (mut actor, mut egress_rx, runtime_stats) = test_actor(config);
        let now = Instant::now() + Duration::from_secs(1);
        let blocked = node(1, "8.8.8.8:6881");
        let eligible = node(2, "1.1.1.1:6881");

        assert_eq!(actor.pool.admit(blocked, now), AdmissionOutcome::Admitted);
        assert_eq!(actor.pool.admit(eligible, now), AdmissionOutcome::Admitted);
        actor
            .subnet_in_flight
            .insert(SubnetKey::from_addr(&blocked.addr), max_per_subnet);

        assert!(actor.schedule_one(now));
        let request = egress_rx.try_recv().expect("eligible node was dispatched");

        assert_eq!(request.node, eligible);
        assert_eq!(actor.pool.front(), Some(blocked));
        assert_eq!(actor.pool.admit(blocked, now), AdmissionOutcome::Duplicate);
        assert!(!actor.pool.contains_recent(&blocked.addr, now));
        assert!(actor.pool.contains_recent(&eligible.addr, now));
        let snapshot = runtime_stats.snapshot();
        assert_eq!(snapshot.queries_new, 1);
        assert_eq!(snapshot.find_node_in_flight, 1);
        assert_eq!(snapshot.find_node_in_flight_max, 512);
    }

    #[test]
    fn runtime_stats_count_dropped_crawl_events() {
        let mut options = CrawlOptions::default();
        options.scheduler.priority_event_channel_capacity = 1;
        options.scheduler.discovery_event_channel_capacity = 1;
        let config = ResolvedCrawlConfig::from_options(&options);
        let stats = DhtRuntimeStats::with_limits(DhtRuntimeLimits {
            metadata_queue: 100,
            node_pool: config.pool_capacity,
            node_pool_low_watermark: config.low_watermark,
            find_node_in_flight: config.max_in_flight,
            initial_find_node_rate: config.max_find_node_rate_per_sec,
            hash_ingress_queue: 0,
            crawl_priority_queue: config.priority_event_channel_capacity,
            crawl_discovery_queue: config.discovery_event_channel_capacity,
        });
        let engine = CrawlEngine::new(config, stats.clone());

        engine.route_discovered(node(1, "8.8.8.8:1"));
        engine.route_discovered(node(2, "1.1.1.1:2"));
        let response = || DhtResponse {
            id: None,
            nodes: None,
            nodes6: None,
            values: None,
            ..Default::default()
        };
        engine.route_response("8.8.8.8:1".parse().unwrap(), [1; 8], response());
        engine.route_response("1.1.1.1:2".parse().unwrap(), [2; 8], response());

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.crawl_events_dropped_discovered, 1);
        assert_eq!(snapshot.crawl_events_dropped_response, 1);
        assert_eq!(snapshot.crawl_discovery_queue_depth, 1);
        assert_eq!(snapshot.crawl_discovery_queue_capacity, 1);
        assert_eq!(snapshot.crawl_priority_queue_depth, 1);
        assert_eq!(snapshot.crawl_priority_queue_capacity, 1);
    }
}
