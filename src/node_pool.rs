use crate::addr::is_valid_node_addr;
use crate::budget::RateBucket;
use crate::types::NodeTuple;
use ahash::{AHashMap, AHashSet};
use std::collections::{BTreeSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdmissionOutcome {
    Admitted,
    Replaced,
    Duplicate,
    RateLimited,
    Invalid,
}

#[derive(Debug, Clone, Copy)]
struct QueuedNode {
    node: NodeTuple,
    #[cfg_attr(not(feature = "metrics"), allow(dead_code))]
    queued_at: Instant,
}

pub(crate) struct NodePool {
    queue: VecDeque<QueuedNode>,
    queued: AHashSet<SocketAddr>,
    recent: AHashMap<SocketAddr, Instant>,
    recent_expiry: VecDeque<(Instant, SocketAddr)>,
    replacement_budget: RateBucket,
    recent_ttl: Duration,
    capacity: usize,
    warmed: bool,
}

impl NodePool {
    pub(crate) fn new(
        capacity: usize,
        replacements_per_minute: u32,
        recent_ttl: Duration,
        now: Instant,
    ) -> Self {
        let capacity = capacity.max(1);
        let replacement_burst = replacements_per_minute.div_ceil(60).max(1);
        Self {
            queue: VecDeque::with_capacity(capacity),
            queued: AHashSet::with_capacity(capacity),
            recent: AHashMap::with_capacity(capacity),
            recent_expiry: VecDeque::with_capacity(capacity),
            replacement_budget: RateBucket::per_minute(
                replacements_per_minute,
                replacement_burst,
                true,
                now,
            ),
            recent_ttl,
            capacity,
            warmed: false,
        }
    }

    pub(crate) fn admit(&mut self, node: NodeTuple, now: Instant) -> AdmissionOutcome {
        if !is_valid_node_addr(&node.addr) {
            return AdmissionOutcome::Invalid;
        }
        self.expire_recent(now);
        if self.queued.contains(&node.addr) || self.recent.contains_key(&node.addr) {
            return AdmissionOutcome::Duplicate;
        }
        if self.warmed && !self.replacement_budget.try_take_one(now) {
            return AdmissionOutcome::RateLimited;
        }

        let replaced = if self.queue.len() >= self.capacity {
            self.pop_front_internal().is_some()
        } else {
            false
        };
        self.queued.insert(node.addr);
        self.queue.push_back(QueuedNode {
            node,
            queued_at: now,
        });
        if self.queue.len() >= self.capacity {
            self.warmed = true;
        }
        if replaced {
            AdmissionOutcome::Replaced
        } else {
            AdmissionOutcome::Admitted
        }
    }

    pub(crate) fn front(&self) -> Option<NodeTuple> {
        self.queue.front().map(|entry| entry.node)
    }

    /// Move the FIFO head behind the remaining queued nodes without marking
    /// it as probed. The address stays in `queued` and is not added to `recent`.
    pub(crate) fn rotate_front_to_back(&mut self) -> bool {
        if self.queue.len() <= 1 {
            return false;
        }
        self.queue.rotate_left(1);
        true
    }

    pub(crate) fn take_front_for_probe(&mut self, now: Instant) -> Option<NodeTuple> {
        let entry = self.pop_front_internal()?;
        let expires_at = now + self.recent_ttl;
        self.recent.insert(entry.node.addr, expires_at);
        self.recent_expiry.push_back((expires_at, entry.node.addr));
        Some(entry.node)
    }

    pub(crate) fn restore_front(&mut self, node: NodeTuple, queued_at: Instant) {
        self.recent.remove(&node.addr);
        self.queued.insert(node.addr);
        self.queue.push_front(QueuedNode { node, queued_at });
    }

    pub(crate) fn contains_recent(&mut self, addr: &SocketAddr, now: Instant) -> bool {
        self.expire_recent(now);
        self.recent.contains_key(addr)
    }

    pub(crate) fn record_probe(&mut self, addr: SocketAddr, now: Instant) {
        let expires_at = now + self.recent_ttl;
        self.recent.insert(addr, expires_at);
        self.recent_expiry.push_back((expires_at, addr));
    }

    pub(crate) fn len(&self) -> usize {
        self.queue.len()
    }

    #[cfg_attr(not(feature = "metrics"), allow(dead_code))]
    pub(crate) fn oldest_age(&self, now: Instant) -> Duration {
        self.queue
            .front()
            .and_then(|entry| now.checked_duration_since(entry.queued_at))
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn is_warmed(&self) -> bool {
        self.warmed
    }

    fn pop_front_internal(&mut self) -> Option<QueuedNode> {
        let entry = self.queue.pop_front()?;
        self.queued.remove(&entry.node.addr);
        Some(entry)
    }

    fn expire_recent(&mut self, now: Instant) {
        while let Some((expires_at, addr)) = self.recent_expiry.front().copied() {
            if expires_at > now {
                break;
            }
            self.recent_expiry.pop_front();
            if self.recent.get(&addr).copied() == Some(expires_at) {
                self.recent.remove(&addr);
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ResponsiveEntry {
    node: NodeTuple,
    expires_at: Instant,
    last_probe_at: Instant,
    revisit_key: Option<(Instant, u64, usize)>,
}

/// Fixed-size responsive-node ring. The crawl actor is the only writer.
pub(crate) struct ResponsiveReservoir {
    slots: Vec<Option<ResponsiveEntry>>,
    index: AHashMap<SocketAddr, usize>,
    write_cursor: usize,
    revisits: BTreeSet<(Instant, u64, usize)>,
    revisit_sequence: u64,
    ttl: Duration,
    revisit_interval: Duration,
}

impl ResponsiveReservoir {
    pub(crate) fn new(capacity: usize, ttl: Duration, revisit_interval: Duration) -> Self {
        let capacity = capacity.max(1);
        Self {
            slots: vec![None; capacity],
            index: AHashMap::with_capacity(capacity),
            write_cursor: 0,
            revisits: BTreeSet::new(),
            revisit_sequence: 0,
            ttl,
            revisit_interval,
        }
    }

    pub(crate) fn record(&mut self, node: NodeTuple, now: Instant, probed_at: Instant) {
        if let Some(slot) = self.index.get(&node.addr).copied() {
            let entry = self.slots[slot].as_mut().expect("indexed slot exists");
            entry.node = node;
            entry.expires_at = now + self.ttl;
            // Replies refresh liveness, not the time of the last dispatched probe.
            return;
        }

        let slot = self.write_cursor;
        if let Some(old) = self.slots[slot] {
            self.index.remove(&old.node.addr);
            if let Some(key) = old.revisit_key {
                self.revisits.remove(&key);
            }
        }
        self.slots[slot] = Some(ResponsiveEntry {
            node,
            expires_at: now + self.ttl,
            last_probe_at: probed_at,
            revisit_key: None,
        });
        self.index.insert(node.addr, slot);
        self.enqueue_revisit(slot, probed_at + self.revisit_interval);
        self.write_cursor = (self.write_cursor + 1) % self.slots.len();
    }

    /// Reserve one eligible node. The caller must record a successful dispatch
    /// or defer it after a failure before selecting another candidate.
    pub(crate) fn next_revisit(&mut self, now: Instant) -> Option<NodeTuple> {
        // A cold reservoir returns immediately, and expiry cleanup has a fixed
        // per-call bound rather than scanning the entire reservoir on every tick.
        for _ in 0..256 {
            let key = self.revisits.first().copied()?;
            if key.0 > now {
                return None;
            }
            self.revisits.remove(&key);
            let slot = key.2;
            let entry = self.slots[slot].as_mut().expect("scheduled slot exists");
            entry.revisit_key = None;
            if entry.expires_at <= now {
                self.index.remove(&entry.node.addr);
                self.slots[slot] = None;
                continue;
            }
            return Some(entry.node);
        }
        None
    }

    pub(crate) fn record_probe(&mut self, addr: SocketAddr, now: Instant) {
        let Some(slot) = self.index.get(&addr).copied() else {
            return;
        };
        self.slots[slot]
            .as_mut()
            .expect("indexed slot exists")
            .last_probe_at = now;
        self.enqueue_revisit(slot, now + self.revisit_interval);
    }

    /// Briefly yield a candidate whose dispatch was blocked, without claiming
    /// that another probe was sent or imposing the full revisit cooldown.
    pub(crate) fn defer_revisit(&mut self, addr: SocketAddr, retry_at: Instant) {
        let Some(slot) = self.index.get(&addr).copied() else {
            return;
        };
        let entry = self.slots[slot].expect("indexed slot exists");
        self.enqueue_revisit(
            slot,
            retry_at.max(entry.last_probe_at + self.revisit_interval),
        );
    }

    fn enqueue_revisit(&mut self, slot: usize, when: Instant) {
        let entry = self.slots[slot].as_mut().expect("scheduled slot exists");
        if let Some(key) = entry.revisit_key {
            self.revisits.remove(&key);
        }
        let key = (when, self.revisit_sequence, slot);
        self.revisit_sequence = self.revisit_sequence.wrapping_add(1);
        entry.revisit_key = Some(key);
        self.revisits.insert(key);
    }

    pub(crate) fn snapshot(&self, limit: usize, now: Instant) -> Vec<NodeTuple> {
        self.slots
            .iter()
            .filter_map(|entry| {
                entry
                    .filter(|entry| entry.expires_at > now)
                    .map(|entry| entry.node)
            })
            .take(limit)
            .collect()
    }
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub(crate) enum SubnetKey {
    V4([u8; 3]),
    V6([u8; 8]),
}

impl SubnetKey {
    pub(crate) fn from_addr(addr: &SocketAddr) -> Self {
        match addr.ip() {
            IpAddr::V4(ip) => {
                let octets = ip.octets();
                Self::V4([octets[0], octets[1], octets[2]])
            }
            IpAddr::V6(ip) => {
                let octets = ip.octets();
                Self::V6(octets[..8].try_into().expect("IPv6 prefix has eight bytes"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn node(id: u8, addr: &str) -> NodeTuple {
        NodeTuple {
            id: [id; 20],
            addr: addr.parse().unwrap(),
        }
    }

    #[test]
    fn strict_fifo_replaces_oldest_after_warmup() {
        let start = Instant::now();
        let mut pool = NodePool::new(2, 600, Duration::from_secs(60), start);
        let first = node(1, "8.8.8.8:1");
        let second = node(2, "1.1.1.1:2");
        let third = node(3, "9.9.9.9:3");
        assert_eq!(pool.admit(first, start), AdmissionOutcome::Admitted);
        assert_eq!(pool.admit(second, start), AdmissionOutcome::Admitted);
        assert!(pool.is_warmed());
        assert_eq!(pool.admit(third, start), AdmissionOutcome::Replaced);
        assert_eq!(pool.front(), Some(second));
    }

    #[test]
    fn duplicate_does_not_reorder_fifo() {
        let start = Instant::now();
        let mut pool = NodePool::new(3, 600, Duration::from_secs(60), start);
        let first = node(1, "8.8.8.8:1");
        let second = node(2, "1.1.1.1:2");
        pool.admit(first, start);
        pool.admit(second, start);
        assert_eq!(pool.admit(first, start), AdmissionOutcome::Duplicate);
        assert_eq!(pool.front(), Some(first));
    }

    #[test]
    fn rotating_front_preserves_queued_dedup_and_recent_state() {
        let start = Instant::now();
        let mut pool = NodePool::new(3, 600, Duration::from_secs(60), start);
        let first = node(1, "8.8.8.8:1");
        let second = node(2, "1.1.1.1:2");

        assert_eq!(pool.admit(first, start), AdmissionOutcome::Admitted);
        assert_eq!(pool.admit(second, start), AdmissionOutcome::Admitted);
        assert!(pool.rotate_front_to_back());

        assert_eq!(pool.front(), Some(second));
        assert_eq!(pool.admit(first, start), AdmissionOutcome::Duplicate);
        assert!(!pool.contains_recent(&first.addr, start));
    }

    #[test]
    fn warmed_pool_enforces_replacement_rate() {
        let start = Instant::now();
        let mut pool = NodePool::new(2, 60, Duration::from_secs(60), start);
        pool.admit(node(1, "8.8.8.8:1"), start);
        pool.admit(node(2, "1.1.1.1:2"), start);
        assert_eq!(
            pool.admit(node(3, "9.9.9.9:3"), start),
            AdmissionOutcome::Replaced
        );
        assert_eq!(
            pool.admit(node(4, "208.67.222.222:4"), start),
            AdmissionOutcome::RateLimited
        );
        assert_eq!(
            pool.admit(node(4, "208.67.222.222:4"), start + Duration::from_secs(1)),
            AdmissionOutcome::Replaced
        );
    }

    #[test]
    fn recent_probe_blocks_readmission_until_expiry() {
        let start = Instant::now();
        let mut pool = NodePool::new(3, 600, Duration::from_secs(10), start);
        let first = node(1, "8.8.8.8:1");
        pool.admit(first, start);
        assert_eq!(pool.take_front_for_probe(start), Some(first));
        assert_eq!(pool.admit(first, start), AdmissionOutcome::Duplicate);
        assert_eq!(
            pool.admit(first, start + Duration::from_secs(11)),
            AdmissionOutcome::Admitted
        );
    }

    #[test]
    fn responsive_ring_overwrites_without_growing() {
        let start = Instant::now();
        let mut reservoir = ResponsiveReservoir::new(2, Duration::from_secs(10), Duration::ZERO);
        reservoir.record(node(1, "8.8.8.8:1"), start, start);
        reservoir.record(node(2, "1.1.1.1:2"), start, start);
        reservoir.record(node(3, "9.9.9.9:3"), start, start);
        let snapshot = reservoir.snapshot(10, start);
        assert_eq!(snapshot.len(), 2);
        assert!(!snapshot.iter().any(|entry| entry.id == [1; 20]));
        assert_eq!(reservoir.revisits.len(), 2);
    }

    #[test]
    fn revisit_cooldown_starts_at_dispatch_and_replies_do_not_extend_it() {
        let start = Instant::now();
        let mut reservoir =
            ResponsiveReservoir::new(2, Duration::from_secs(90), Duration::from_secs(30));
        let first = node(1, "8.8.8.8:1");
        reservoir.record(first, start + Duration::from_secs(2), start);
        assert_eq!(
            reservoir.next_revisit(start + Duration::from_secs(29)),
            None
        );
        let dispatched = start + Duration::from_secs(30);
        assert_eq!(reservoir.next_revisit(dispatched), Some(first));
        reservoir.record_probe(first.addr, dispatched);
        reservoir.record(first, start + Duration::from_secs(32), dispatched);
        assert_eq!(
            reservoir.next_revisit(start + Duration::from_secs(59)),
            None
        );
        assert_eq!(
            reservoir.next_revisit(start + Duration::from_secs(60)),
            Some(first)
        );
    }

    #[test]
    fn zero_revisit_interval_keeps_immediate_round_robin() {
        let start = Instant::now();
        let mut reservoir = ResponsiveReservoir::new(2, Duration::from_secs(10), Duration::ZERO);
        let first = node(1, "8.8.8.8:1");
        let second = node(2, "1.1.1.1:2");
        reservoir.record(first, start, start);
        reservoir.record(second, start, start);
        for expected in [first, second, first, second] {
            assert_eq!(reservoir.next_revisit(start), Some(expected));
            reservoir.record_probe(expected.addr, start);
            assert_eq!(reservoir.revisits.len(), 2);
        }
    }

    #[test]
    fn revisit_expiry_cleanup_is_bounded_and_removes_time_index_entries() {
        let start = Instant::now();
        let mut reservoir = ResponsiveReservoir::new(300, Duration::from_secs(10), Duration::ZERO);
        for port in 1..=300 {
            reservoir.record(node(1, &format!("8.8.8.8:{port}")), start, start);
        }
        assert_eq!(
            reservoir.next_revisit(start + Duration::from_secs(11)),
            None
        );
        assert_eq!(reservoir.revisits.len(), 44);
        assert_eq!(
            reservoir.next_revisit(start + Duration::from_secs(11)),
            None
        );
        assert!(reservoir.revisits.is_empty());
        assert!(reservoir.index.is_empty());
    }

    #[test]
    #[ignore = "release-only FIFO throughput smoke test"]
    fn million_fifo_operations() {
        let start = Instant::now();
        let mut pool = NodePool::new(100_000, u32::MAX, Duration::from_secs(600), start);
        for value in 0..1_000_000u32 {
            let octets = value.to_be_bytes();
            let node = NodeTuple {
                id: [octets[3]; 20],
                addr: SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(11, octets[1], octets[2], octets[3])),
                    (value % 65_534 + 1) as u16,
                ),
            };
            let outcome = pool.admit(node, start);
            assert!(!matches!(outcome, AdmissionOutcome::RateLimited));
        }
        assert_eq!(pool.len(), 100_000);
        eprintln!("1,000,000 FIFO admissions in {:?}", start.elapsed());
    }
}
