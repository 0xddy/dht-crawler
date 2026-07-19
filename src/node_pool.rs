use crate::addr::is_valid_node_addr;
use crate::budget::RateBucket;
use crate::types::NodeTuple;
use ahash::{AHashMap, AHashSet};
use std::collections::VecDeque;
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
}

/// Fixed-size responsive-node ring. The crawl actor is the only writer.
pub(crate) struct ResponsiveReservoir {
    slots: Vec<Option<ResponsiveEntry>>,
    index: AHashMap<SocketAddr, usize>,
    write_cursor: usize,
    revisit_cursor: usize,
    ttl: Duration,
}

impl ResponsiveReservoir {
    pub(crate) fn new(capacity: usize, ttl: Duration) -> Self {
        let capacity = capacity.max(1);
        Self {
            slots: vec![None; capacity],
            index: AHashMap::with_capacity(capacity),
            write_cursor: 0,
            revisit_cursor: 0,
            ttl,
        }
    }

    pub(crate) fn record(&mut self, node: NodeTuple, now: Instant) {
        let entry = ResponsiveEntry {
            node,
            expires_at: now + self.ttl,
        };
        if let Some(slot) = self.index.get(&node.addr).copied() {
            self.slots[slot] = Some(entry);
            return;
        }

        let slot = self.write_cursor;
        if let Some(old) = self.slots[slot]
            && self.index.get(&old.node.addr).copied() == Some(slot)
        {
            self.index.remove(&old.node.addr);
        }
        self.slots[slot] = Some(entry);
        self.index.insert(node.addr, slot);
        self.write_cursor = (self.write_cursor + 1) % self.slots.len();
    }

    pub(crate) fn next_revisit(&mut self, now: Instant) -> Option<NodeTuple> {
        for _ in 0..self.slots.len() {
            let slot = self.revisit_cursor;
            self.revisit_cursor = (self.revisit_cursor + 1) % self.slots.len();
            let Some(entry) = self.slots[slot] else {
                continue;
            };
            if entry.expires_at <= now {
                if self.index.get(&entry.node.addr).copied() == Some(slot) {
                    self.index.remove(&entry.node.addr);
                }
                self.slots[slot] = None;
                continue;
            }
            return Some(entry.node);
        }
        None
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
        let mut reservoir = ResponsiveReservoir::new(2, Duration::from_secs(10));
        reservoir.record(node(1, "8.8.8.8:1"), start);
        reservoir.record(node(2, "1.1.1.1:2"), start);
        reservoir.record(node(3, "9.9.9.9:3"), start);
        let snapshot = reservoir.snapshot(10, start);
        assert_eq!(snapshot.len(), 2);
        assert!(!snapshot.iter().any(|entry| entry.id == [1; 20]));
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
