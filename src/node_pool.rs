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

#[derive(Clone, Copy)]
struct RecentProbe {
    expires_at: Instant,
    heap_index: usize,
}

/// One map entry and one heap entry per endpoint. Repeated probes only refresh
/// the map deadline; the heap deadline is lazily advanced when it becomes due.
struct RecentProbes {
    entries: AHashMap<SocketAddr, RecentProbe>,
    expiry: Vec<(Instant, SocketAddr)>,
    capacity: usize,
}

impl RecentProbes {
    fn new(capacity: usize) -> Self {
        Self {
            entries: AHashMap::new(),
            expiry: Vec::new(),
            capacity: capacity.max(1),
        }
    }

    fn contains(&self, addr: &SocketAddr, now: Instant) -> bool {
        self.entries
            .get(addr)
            .is_some_and(|entry| entry.expires_at > now)
    }

    fn record(&mut self, addr: SocketAddr, expires_at: Instant) {
        if let Some(entry) = self.entries.get_mut(&addr) {
            entry.expires_at = expires_at;
            return;
        }
        if self.entries.len() == self.capacity {
            // A hard safety bound for callers that exceed the destination budget.
            // The actor includes resident responsive/bootstrap probes as well as
            // the rate window; ordinary rate-compliant calls do not evict live entries.
            self.remove_at(0);
        }
        let heap_index = self.expiry.len();
        self.expiry.push((expires_at, addr));
        self.entries.insert(
            addr,
            RecentProbe {
                expires_at,
                heap_index,
            },
        );
        self.sift_up(heap_index);
    }

    fn remove(&mut self, addr: &SocketAddr) {
        if let Some(entry) = self.entries.get(addr) {
            self.remove_at(entry.heap_index);
        }
    }

    fn swap(&mut self, left: usize, right: usize) {
        self.expiry.swap(left, right);
        self.entries
            .get_mut(&self.expiry[left].1)
            .unwrap()
            .heap_index = left;
        self.entries
            .get_mut(&self.expiry[right].1)
            .unwrap()
            .heap_index = right;
    }

    fn sift_up(&mut self, mut index: usize) {
        while index > 0 {
            let parent = (index - 1) / 2;
            if self.expiry[parent] <= self.expiry[index] {
                break;
            }
            self.swap(parent, index);
            index = parent;
        }
    }

    fn sift_down(&mut self, mut index: usize) {
        loop {
            let left = index * 2 + 1;
            if left >= self.expiry.len() {
                break;
            }
            let right = left + 1;
            let child = if right < self.expiry.len() && self.expiry[right] < self.expiry[left] {
                right
            } else {
                left
            };
            if self.expiry[index] <= self.expiry[child] {
                break;
            }
            self.swap(index, child);
            index = child;
        }
    }

    fn remove_at(&mut self, index: usize) {
        let removed = self.expiry.swap_remove(index);
        self.entries.remove(&removed.1);
        if index < self.expiry.len() {
            self.entries
                .get_mut(&self.expiry[index].1)
                .unwrap()
                .heap_index = index;
            if index > 0 && self.expiry[index] < self.expiry[(index - 1) / 2] {
                self.sift_up(index);
            } else {
                self.sift_down(index);
            }
        }
    }

    fn has_due(&self, now: Instant) -> bool {
        self.expiry.first().is_some_and(|entry| entry.0 <= now)
    }

    fn expire(&mut self, now: Instant) -> usize {
        let mut removed = 0;
        // Never monopolize a crawl actor iteration after a long scheduling pause.
        for _ in 0..256 {
            let Some(&(check_at, addr)) = self.expiry.first() else {
                break;
            };
            if check_at > now {
                break;
            }
            let expires_at = self.entries[&addr].expires_at;
            if expires_at <= now {
                self.remove_at(0);
                removed += 1;
            } else {
                self.expiry[0].0 = expires_at;
                self.sift_down(0);
            }
        }
        removed
    }
}

pub(crate) struct NodePool {
    queue: VecDeque<QueuedNode>,
    queued: AHashSet<SocketAddr>,
    recent: RecentProbes,
    recent_expired: usize,
    recent_expiry_micros: u64,
    recent_expiry_calls: u64,
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
        recent_capacity: usize,
        now: Instant,
    ) -> Self {
        let capacity = capacity.max(1);
        let replacement_burst = replacements_per_minute.div_ceil(60).max(1);
        Self {
            queue: VecDeque::with_capacity(capacity),
            queued: AHashSet::with_capacity(capacity),
            recent: RecentProbes::new(recent_capacity),
            recent_expired: 0,
            recent_expiry_micros: 0,
            recent_expiry_calls: 0,
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
        if self.queued.contains(&node.addr) || self.recent.contains(&node.addr, now) {
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
        self.prepare_recent_slot(&entry.node.addr, now);
        self.recent.record(entry.node.addr, expires_at);
        Some(entry.node)
    }

    pub(crate) fn restore_front(&mut self, node: NodeTuple, queued_at: Instant) {
        self.recent.remove(&node.addr);
        self.queued.insert(node.addr);
        self.queue.push_front(QueuedNode { node, queued_at });
    }

    pub(crate) fn contains_recent(&mut self, addr: &SocketAddr, now: Instant) -> bool {
        self.expire_recent(now);
        self.recent.contains(addr, now)
    }

    pub(crate) fn record_probe(&mut self, addr: SocketAddr, now: Instant) {
        let expires_at = now + self.recent_ttl;
        self.prepare_recent_slot(&addr, now);
        self.recent.record(addr, expires_at);
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
        if self.recent.has_due(now) {
            let started = Instant::now();
            self.recent_expired += self.recent.expire(now);
            self.recent_expiry_micros = self
                .recent_expiry_micros
                .saturating_add(started.elapsed().as_micros().min(u64::MAX as u128) as u64);
            self.recent_expiry_calls += 1;
        }
    }

    fn prepare_recent_slot(&mut self, addr: &SocketAddr, now: Instant) {
        self.expire_recent(now);
        // Normal expiry work is bounded per call. At the hard capacity, finish
        // due checks before considering eviction: lazily refreshed heap heads
        // must not hide an expired neighbor and evict a still-recent endpoint.
        while self.recent.entries.len() >= self.recent.capacity
            && !self.recent.entries.contains_key(addr)
            && self.recent.has_due(now)
        {
            self.expire_recent(now);
        }
    }

    pub(crate) fn recent_state(&self) -> (usize, usize, usize) {
        (
            self.recent.entries.len(),
            self.recent.capacity,
            self.recent.expiry.len(),
        )
    }

    pub(crate) fn take_recent_expiry_stats(&mut self) -> (usize, u64, u64) {
        (
            std::mem::take(&mut self.recent_expired),
            std::mem::take(&mut self.recent_expiry_micros),
            std::mem::take(&mut self.recent_expiry_calls),
        )
    }
}

#[derive(Debug, Clone, Copy)]
struct ResponsiveEntry {
    node: NodeTuple,
    expires_at: Instant,
    last_probe_at: Instant,
    last_success_at: Instant,
    failures: u8,
    blocked_until: Option<Instant>,
    revisit_key: Option<(Instant, u64, usize)>,
    ready: bool,
    previous: Option<usize>,
    next: Option<usize>,
}

/// Fixed-size responsive-node ring. The crawl actor is the only writer.
pub(crate) struct ResponsiveReservoir {
    slots: Vec<Option<ResponsiveEntry>>,
    index: AHashMap<SocketAddr, usize>,
    write_cursor: usize,
    revisits: BTreeSet<(Instant, u64, usize)>,
    revisit_sequence: u64,
    ready_head: Option<usize>,
    ready_tail: Option<usize>,
    ready_len: usize,
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
            ready_head: None,
            ready_tail: None,
            ready_len: 0,
            ttl,
            revisit_interval,
        }
    }

    pub(crate) fn record(&mut self, node: NodeTuple, now: Instant, probed_at: Instant) {
        if let Some(slot) = self.index.get(&node.addr).copied() {
            let entry = self.slots[slot].as_mut().expect("indexed slot exists");
            entry.node = node;
            entry.expires_at = now + self.ttl;
            entry.last_success_at = now;
            // Replies refresh liveness, not the time of the last dispatched probe.
            if entry.failures > 0 {
                entry.failures = 0;
                entry.blocked_until = None;
                let eligible_at = entry.last_probe_at + self.revisit_interval;
                self.enqueue_revisit(slot, eligible_at, now);
            }
            return;
        }

        let slot = self.write_cursor;
        if let Some(old) = self.slots[slot] {
            self.unschedule(slot);
            self.index.remove(&old.node.addr);
        }
        self.slots[slot] = Some(ResponsiveEntry {
            node,
            expires_at: now + self.ttl,
            last_probe_at: probed_at,
            last_success_at: now,
            failures: 0,
            blocked_until: None,
            revisit_key: None,
            ready: false,
            previous: None,
            next: None,
        });
        self.index.insert(node.addr, slot);
        self.enqueue_revisit(slot, probed_at + self.revisit_interval, now);
        self.write_cursor = (self.write_cursor + 1) % self.slots.len();
    }

    /// Reserve one eligible node. The caller must record a successful dispatch
    /// or defer it after a failure before selecting another candidate.
    pub(crate) fn next_revisit(&mut self, now: Instant) -> Option<NodeTuple> {
        // Only delayed endpoints use the tree. Healthy zero-interval revisits
        // rotate through the intrusive ready list without allocations or O(log N) work.
        for _ in 0..256 {
            let Some(key) = self.revisits.first().copied() else {
                break;
            };
            if key.0 > now {
                break;
            }
            self.revisits.remove(&key);
            let slot = key.2;
            self.slots[slot].as_mut().unwrap().revisit_key = None;
            self.push_ready(slot);
        }
        for _ in 0..256 {
            let slot = self.ready_head?;
            self.unschedule(slot);
            let entry = self.slots[slot].as_mut().expect("scheduled slot exists");
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
        self.enqueue_revisit(slot, now + self.revisit_interval, now);
    }

    /// Briefly yield a candidate whose dispatch was blocked, without claiming
    /// that another probe was sent or imposing the full revisit cooldown.
    pub(crate) fn defer_revisit(&mut self, addr: SocketAddr, retry_at: Instant, now: Instant) {
        let Some(slot) = self.index.get(&addr).copied() else {
            return;
        };
        let entry = self.slots[slot].expect("indexed slot exists");
        self.enqueue_revisit(
            slot,
            retry_at.max(entry.last_probe_at + self.revisit_interval),
            now,
        );
    }

    /// Back off actual failed probes, including when the configured healthy
    /// revisit interval is zero. A newer successful reply supersedes old failures.
    pub(crate) fn record_failure(
        &mut self,
        addr: SocketAddr,
        dispatched_at: Instant,
        now: Instant,
    ) -> bool {
        let Some(slot) = self.index.get(&addr).copied() else {
            return false;
        };
        let entry = self.slots[slot].as_mut().unwrap();
        if entry.last_success_at > dispatched_at {
            return false;
        }
        entry.failures = entry.failures.saturating_add(1).min(5);
        let seconds = (5u64 << (entry.failures - 1)).min(60);
        entry.blocked_until = Some(now + Duration::from_secs(seconds));
        let eligible_at =
            (entry.last_probe_at + self.revisit_interval).max(entry.blocked_until.unwrap());
        self.enqueue_revisit(slot, eligible_at, now);
        true
    }

    pub(crate) fn is_backing_off(&self, addr: &SocketAddr, now: Instant) -> bool {
        self.index
            .get(addr)
            .and_then(|slot| self.slots[*slot])
            .and_then(|entry| entry.blocked_until)
            .is_some_and(|deadline| deadline > now)
    }

    fn unschedule(&mut self, slot: usize) {
        let entry = self.slots[slot].unwrap();
        if let Some(key) = entry.revisit_key {
            self.revisits.remove(&key);
        }
        if entry.ready {
            if let Some(previous) = entry.previous {
                self.slots[previous].as_mut().unwrap().next = entry.next;
            } else {
                self.ready_head = entry.next;
            }
            if let Some(next) = entry.next {
                self.slots[next].as_mut().unwrap().previous = entry.previous;
            } else {
                self.ready_tail = entry.previous;
            }
            self.ready_len -= 1;
        }
        let entry = self.slots[slot].as_mut().unwrap();
        entry.ready = false;
        entry.previous = None;
        entry.next = None;
        entry.revisit_key = None;
    }

    fn push_ready(&mut self, slot: usize) {
        let entry = self.slots[slot].as_mut().unwrap();
        entry.ready = true;
        entry.previous = self.ready_tail;
        entry.next = None;
        if let Some(tail) = self.ready_tail {
            self.slots[tail].as_mut().unwrap().next = Some(slot);
        } else {
            self.ready_head = Some(slot);
        }
        self.ready_tail = Some(slot);
        self.ready_len += 1;
    }

    fn enqueue_revisit(&mut self, slot: usize, when: Instant, now: Instant) {
        self.unschedule(slot);
        let when = self.slots[slot]
            .unwrap()
            .blocked_until
            .map_or(when, |blocked| when.max(blocked));
        if when <= now {
            self.push_ready(slot);
            return;
        }
        let key = (when, self.revisit_sequence, slot);
        self.revisit_sequence = self.revisit_sequence.wrapping_add(1);
        self.slots[slot].as_mut().unwrap().revisit_key = Some(key);
        self.revisits.insert(key);
    }

    pub(crate) fn snapshot(&self, limit: usize, now: Instant) -> Vec<NodeTuple> {
        self.slots
            .iter()
            .filter_map(|entry| {
                entry
                    .filter(|entry| {
                        entry.expires_at > now
                            && entry.blocked_until.is_none_or(|deadline| deadline <= now)
                    })
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
    use rand::{Rng, SeedableRng};
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
        let mut pool = NodePool::new(2, 600, Duration::from_secs(60), 1000, start);
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
        let mut pool = NodePool::new(3, 600, Duration::from_secs(60), 1000, start);
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
        let mut pool = NodePool::new(3, 600, Duration::from_secs(60), 1000, start);
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
        let mut pool = NodePool::new(2, 60, Duration::from_secs(60), 1000, start);
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
        let mut pool = NodePool::new(3, 600, Duration::from_secs(10), 1000, start);
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
    fn repeated_recent_probes_keep_exactly_one_expiry_and_restore_removes_it() {
        let start = Instant::now();
        let mut pool = NodePool::new(3, 600, Duration::from_secs(10), 100, start);
        let first = node(1, "8.8.8.8:1");
        for offset in 0..100_000 {
            pool.record_probe(first.addr, start + Duration::from_micros(offset));
        }
        assert_eq!(pool.recent_state(), (1, 100, 1));
        assert!(pool.contains_recent(&first.addr, start + Duration::from_secs(10)));
        assert!(!pool.contains_recent(&first.addr, start + Duration::from_secs(11)));
        assert_eq!(pool.recent_state(), (0, 100, 0));
        pool.admit(first, start + Duration::from_secs(12));
        let taken = pool
            .take_front_for_probe(start + Duration::from_secs(12))
            .unwrap();
        pool.restore_front(taken, start + Duration::from_secs(12));
        assert_eq!(pool.recent_state(), (0, 100, 0));
    }

    #[test]
    fn lazy_recent_refresh_does_not_hide_other_expired_addresses() {
        let start = Instant::now();
        let first = node(1, "8.8.8.8:1").addr;
        let second = node(2, "1.1.1.1:2").addr;
        let mut recent = RecentProbes::new(2);
        recent.record(first, start + Duration::from_secs(10));
        recent.record(second, start + Duration::from_secs(11));
        recent.record(first, start + Duration::from_secs(20));
        assert_eq!(recent.expire(start + Duration::from_secs(12)), 1);
        assert!(recent.contains(&first, start + Duration::from_secs(12)));
        assert!(!recent.contains(&second, start + Duration::from_secs(12)));
        assert_eq!(recent.expiry.len(), 1);
    }

    #[test]
    fn capacity_pressure_drains_due_checks_before_evicting_refreshed_addresses() {
        let start = Instant::now();
        let mut pool = NodePool::new(300, 60, Duration::from_secs(10), 300, start);
        for port in 1..=299 {
            pool.record_probe(SocketAddr::from(([8, 8, 8, 8], port)), start);
        }
        let expired = SocketAddr::from(([1, 1, 1, 1], 1));
        pool.record_probe(expired, start + Duration::from_secs(1));
        for port in 1..=299 {
            pool.record_probe(
                SocketAddr::from(([8, 8, 8, 8], port)),
                start + Duration::from_secs(5),
            );
        }
        let now = start + Duration::from_secs(11);
        let fresh = SocketAddr::from(([9, 9, 9, 9], 1));
        pool.record_probe(fresh, now);
        for port in 1..=299 {
            assert!(pool.contains_recent(&SocketAddr::from(([8, 8, 8, 8], port)), now));
        }
        assert!(!pool.contains_recent(&expired, now));
        assert!(pool.contains_recent(&fresh, now));
        assert_eq!(pool.recent_state(), (300, 300, 300));
    }

    #[test]
    fn recent_index_matches_reference_and_never_exceeds_its_capacity() {
        let start = Instant::now();
        let mut recent = RecentProbes::new(64);
        let mut reference = AHashMap::new();
        let mut rng = rand::rngs::StdRng::seed_from_u64(73);
        for tick in 0..10_000 {
            let now = start + Duration::from_millis(tick);
            let addr = SocketAddr::from(([8, 8, 8, 8], rng.gen_range(1..=64)));
            if rng.gen_ratio(1, 4) {
                recent.remove(&addr);
                reference.remove(&addr);
            } else {
                let expires = now + Duration::from_millis(30);
                recent.record(addr, expires);
                reference.insert(addr, expires);
            }
            recent.expire(now);
            reference.retain(|_, expires| *expires > now);
            assert_eq!(recent.entries.len(), reference.len());
            assert_eq!(recent.expiry.len(), reference.len());
            for (index, &(check_at, addr)) in recent.expiry.iter().enumerate() {
                assert_eq!(recent.entries[&addr].heap_index, index);
                assert_eq!(recent.entries[&addr].expires_at, reference[&addr]);
                assert!(check_at <= reference[&addr]);
                if index > 0 {
                    assert!(recent.expiry[(index - 1) / 2] <= recent.expiry[index]);
                }
            }
        }
        for port in 1..=1000 {
            recent.record(
                SocketAddr::from(([9, 9, 9, 9], port)),
                start + Duration::from_secs(100),
            );
            assert!(recent.entries.len() <= 64);
            assert_eq!(recent.entries.len(), recent.expiry.len());
        }
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
        assert_eq!(reservoir.ready_len + reservoir.revisits.len(), 2);
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
        for expected in (0..10_000).map(|index| if index % 2 == 0 { first } else { second }) {
            assert_eq!(reservoir.next_revisit(start), Some(expected));
            reservoir.record_probe(expected.addr, start);
            assert_eq!(reservoir.ready_len + reservoir.revisits.len(), 2);
            assert!(
                reservoir.revisits.is_empty(),
                "zero interval must avoid the time index"
            );
        }
    }

    #[test]
    fn responsive_failures_back_off_with_a_cap_and_success_resets_them() {
        let start = Instant::now();
        let first = node(1, "8.8.8.8:1");
        let mut reservoir = ResponsiveReservoir::new(2, Duration::from_secs(900), Duration::ZERO);
        reservoir.record(first, start, start);
        let mut now = start + Duration::from_secs(1);
        for seconds in [5, 10, 20, 40, 60, 60] {
            assert_eq!(reservoir.next_revisit(now), Some(first));
            reservoir.record_probe(first.addr, now);
            assert!(reservoir.record_failure(first.addr, now, now));
            assert_eq!(
                reservoir.next_revisit(now + Duration::from_secs(seconds - 1)),
                None
            );
            now += Duration::from_secs(seconds);
        }
        reservoir.record(first, now, now - Duration::from_secs(1));
        assert_eq!(reservoir.next_revisit(now), Some(first));
        reservoir.record_probe(first.addr, now);
        assert!(reservoir.record_failure(first.addr, now, now));
        assert_eq!(reservoir.next_revisit(now + Duration::from_secs(4)), None);
        assert_eq!(
            reservoir.next_revisit(now + Duration::from_secs(5)),
            Some(first)
        );
    }

    #[test]
    fn old_failure_cannot_override_a_newer_success_or_leave_duplicate_schedule_entries() {
        let start = Instant::now();
        let first = node(1, "8.8.8.8:1");
        let second = node(2, "1.1.1.1:2");
        let mut reservoir = ResponsiveReservoir::new(1, Duration::from_secs(90), Duration::ZERO);
        reservoir.record(first, start + Duration::from_secs(2), start);
        assert!(!reservoir.record_failure(first.addr, start, start + Duration::from_secs(3)));
        assert!(reservoir.record_failure(
            first.addr,
            start + Duration::from_secs(3),
            start + Duration::from_secs(4)
        ));
        reservoir.record(
            second,
            start + Duration::from_secs(5),
            start + Duration::from_secs(5),
        );
        assert!(
            reservoir.revisits.is_empty(),
            "overwriting must remove the old delayed entry"
        );
        assert_eq!(
            reservoir.next_revisit(start + Duration::from_secs(5)),
            Some(second)
        );
        reservoir.record_probe(second.addr, start + Duration::from_secs(5));
        assert_eq!(reservoir.ready_len, 1);
    }

    #[test]
    fn ready_list_stays_bounded_and_fair_after_overwrites_and_delayed_failures() {
        let start = Instant::now();
        let mut reservoir = ResponsiveReservoir::new(3, Duration::from_secs(90), Duration::ZERO);
        let nodes = [
            node(1, "8.8.8.8:1"),
            node(2, "1.1.1.1:2"),
            node(3, "9.9.9.9:3"),
            node(4, "4.4.4.4:4"),
        ];
        for current in &nodes[..3] {
            reservoir.record(*current, start, start);
        }
        assert_eq!(reservoir.next_revisit(start), Some(nodes[0]));
        reservoir.record_probe(nodes[0].addr, start);
        reservoir.record(nodes[3], start, start); // Overwrite the old tail.
        let now = start + Duration::from_secs(1);
        assert!(reservoir.record_failure(nodes[2].addr, now, now)); // Remove the middle.
        for expected in [nodes[1], nodes[3], nodes[1], nodes[3]] {
            assert_eq!(reservoir.next_revisit(now), Some(expected));
            reservoir.record_probe(expected.addr, now);
        }
        let ready_at = now + Duration::from_secs(5);
        let mut found = AHashSet::new();
        for _ in 0..3 {
            let current = reservoir.next_revisit(ready_at).unwrap();
            assert!(found.insert(current.addr));
            reservoir.record_probe(current.addr, ready_at);
        }
        assert_eq!(reservoir.ready_len, 3);
        assert!(reservoir.revisits.is_empty());
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
        assert_eq!(reservoir.ready_len + reservoir.revisits.len(), 44);
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
        let mut pool = NodePool::new(100_000, u32::MAX, Duration::from_secs(600), 100_000, start);
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
