use crate::types::NodeTuple;
use ahash::{AHashMap, AHashSet};
use std::collections::{BTreeSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub(super) struct ResponsiveEntry {
    pub(super) node: NodeTuple,
    pub(super) expires_at: Instant,
    last_probe_at: Instant,
    last_success_at: Instant,
    failures: u8,
    in_flight: bool,
    pub(super) blocked_until: Option<Instant>,
    revisit_key: Option<(Instant, u64, usize)>,
    ready: bool,
    previous: Option<usize>,
    next: Option<usize>,
}

/// Temporary queues share one link per reservoir slot. Moving a consumed slot
/// to a deferred queue reuses its link, so skewed prefixes cannot retain a
/// separate reservoir-sized allocation for each of the 256 prefixes.
#[derive(Clone, Copy)]
struct SnapshotSlots {
    head: usize,
    tail: usize,
    len: usize,
}

impl Default for SnapshotSlots {
    fn default() -> Self {
        Self {
            head: usize::MAX,
            tail: usize::MAX,
            len: 0,
        }
    }
}

impl SnapshotSlots {
    fn push_back(&mut self, slot: usize, links: &mut [usize]) {
        links[slot] = usize::MAX;
        if self.len == 0 {
            self.head = slot;
        } else {
            links[self.tail] = slot;
        }
        self.tail = slot;
        self.len += 1;
    }

    fn pop_front(&mut self, links: &[usize]) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        let slot = self.head;
        self.head = links[slot];
        self.len -= 1;
        Some(slot)
    }

    fn rotate_left(&mut self, count: usize, links: &mut [usize]) {
        if count == 0 {
            return;
        }
        links[self.tail] = self.head;
        for _ in 0..count {
            self.tail = self.head;
            self.head = links[self.head];
        }
        links[self.tail] = usize::MAX;
    }
}

#[derive(Default)]
pub(super) struct SnapshotScratch {
    pub(super) links: Vec<usize>,
    pub(super) subnets: AHashSet<SubnetKey>,
    pub(super) active: VecDeque<usize>,
}

/// Fixed-size responsive-node ring. The crawl actor is the only writer.
pub(crate) struct ResponsiveReservoir {
    pub(super) slots: Vec<Option<ResponsiveEntry>>,
    pub(super) index: AHashMap<SocketAddr, usize>,
    write_cursor: usize,
    pub(super) revisits: BTreeSet<(Instant, u64, usize)>,
    revisit_sequence: u64,
    ready_head: Option<usize>,
    ready_tail: Option<usize>,
    pub(super) ready_len: usize,
    ttl: Duration,
    revisit_interval: Duration,
    snapshot_prefix_cursor: usize,
    snapshot_member_cursors: [usize; 256],
    sparse_prefixes: Vec<u8>,
    pub(super) snapshot_scratch: SnapshotScratch,
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
            snapshot_prefix_cursor: 0,
            snapshot_member_cursors: [0; 256],
            sparse_prefixes: (0..=u8::MAX).collect(),
            snapshot_scratch: SnapshotScratch::default(),
        }
    }

    pub(crate) fn record(&mut self, node: NodeTuple, now: Instant, probed_at: Instant) {
        if let Some(slot) = self.index.get(&node.addr).copied() {
            let entry = self.slots[slot].as_mut().expect("indexed slot exists");
            entry.node = node;
            entry.expires_at = now + self.ttl;
            entry.last_success_at = now;
            let was_in_flight = entry.in_flight;
            entry.in_flight = false;
            // Replies refresh liveness, not the time of the last dispatched probe.
            if entry.failures > 0 {
                entry.failures = 0;
                entry.blocked_until = None;
                let eligible_at = entry.last_probe_at + self.revisit_interval;
                self.enqueue_revisit(slot, eligible_at, now);
            } else if was_in_flight || (!entry.ready && entry.revisit_key.is_none()) {
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
            in_flight: false,
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
        let entry = self.slots[slot].as_mut().expect("indexed slot exists");
        entry.last_probe_at = now;
        entry.in_flight = true;
        self.unschedule(slot);
    }

    /// Briefly yield a candidate whose dispatch was blocked, without claiming
    /// that another probe was sent or imposing the full revisit cooldown.
    pub(crate) fn defer_revisit(&mut self, addr: SocketAddr, retry_at: Instant, now: Instant) {
        let Some(slot) = self.index.get(&addr).copied() else {
            return;
        };
        let entry = self.slots[slot].expect("indexed slot exists");
        if entry.in_flight {
            return;
        }
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
        entry.in_flight = false;
        if entry.last_success_at > dispatched_at {
            if !entry.ready && entry.revisit_key.is_none() {
                let eligible_at = entry.last_probe_at + self.revisit_interval;
                self.enqueue_revisit(slot, eligible_at, now);
            }
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

    /// Publish a rotating, stratified sample rather than the first live slots.
    /// Work and temporary storage are bounded by the reservoir capacity, once
    /// per snapshot refresh; target selection only reads the cached prefixes.
    pub(crate) fn snapshot(&mut self, limit: usize, now: Instant) -> Vec<NodeTuple> {
        let SnapshotScratch {
            links,
            subnets,
            active,
        } = &mut self.snapshot_scratch;
        links.resize(self.slots.len(), usize::MAX);
        subnets.clear();
        active.clear();
        active.reserve(256);
        let mut groups = [SnapshotSlots::default(); 256];
        for (slot, entry) in self.slots.iter().enumerate() {
            if let Some(entry) = entry
                && entry.expires_at > now
                && entry.blocked_until.is_none_or(|deadline| deadline <= now)
            {
                groups[usize::from(entry.node.id[0])].push_back(slot, links);
            }
        }
        let counts: [usize; 256] = std::array::from_fn(|prefix| groups[prefix].len);
        let minimum = *counts.iter().min().unwrap();
        self.sparse_prefixes.clear();
        let mut prefix_order = [0usize; 256];
        let mut prefix_count = 0;
        for (prefix, count) in counts.iter().enumerate() {
            if *count == minimum {
                self.sparse_prefixes.push(prefix as u8);
            }
            if *count > 0 {
                groups[prefix].rotate_left(self.snapshot_member_cursors[prefix] % count, links);
                prefix_order[prefix_count] = prefix;
                prefix_count += 1;
            }
        }

        let limit = limit.min(counts.iter().sum());
        let mut output = Vec::with_capacity(limit);
        let mut selected = [0usize; 256];
        subnets.reserve(limit);
        let mut deferred = [SnapshotSlots::default(); 256];
        let prefix_order = &mut prefix_order[..prefix_count];
        if !prefix_order.is_empty() {
            let rotate = self.snapshot_prefix_cursor % prefix_order.len();
            prefix_order.rotate_left(rotate);
        }

        // Give each represented ID prefix a seed before filling any prefix a
        // second time. Even prefixes sharing one IP subnet remain reachable.
        for prefix in prefix_order.iter().copied() {
            if output.len() < limit
                && let Some(slot) = groups[prefix].pop_front(links)
            {
                let node = self.slots[slot].unwrap().node;
                output.push(node);
                selected[prefix] += 1;
                subnets.insert(SubnetKey::from_addr(&node.addr));
            }
            if groups[prefix].len > 0 {
                active.push_back(prefix);
            }
        }

        // Fill in prefix round-robin order, preferring new /24 or /64 subnets.
        // Each slot is examined at most once here; duplicate subnets wait for
        // the fallback pass instead of being scanned repeatedly.
        while output.len() < limit {
            let Some(prefix) = active.pop_front() else {
                break;
            };
            while let Some(slot) = groups[prefix].pop_front(links) {
                let node = self.slots[slot].unwrap().node;
                if subnets.insert(SubnetKey::from_addr(&node.addr)) {
                    output.push(node);
                    selected[prefix] += 1;
                    break;
                }
                deferred[prefix].push_back(slot, links);
            }
            if groups[prefix].len > 0 {
                active.push_back(prefix);
            }
        }

        // Relax subnet diversity only after all distinct subnet candidates
        // have been considered. This keeps snapshots useful in small networks.
        if output.len() < limit {
            for prefix in prefix_order.iter().copied() {
                if deferred[prefix].len > 0 {
                    active.push_back(prefix);
                }
            }
            while output.len() < limit {
                let Some(prefix) = active.pop_front() else {
                    break;
                };
                let slot = deferred[prefix].pop_front(links).unwrap();
                output.push(self.slots[slot].unwrap().node);
                selected[prefix] += 1;
                if deferred[prefix].len > 0 {
                    active.push_back(prefix);
                }
            }
        }
        for prefix in 0..256 {
            if selected[prefix] > 0 {
                // Advance by one so even when the sample size shares a factor
                // with this group's size, every member gets the first slot.
                self.snapshot_member_cursors[prefix] =
                    (self.snapshot_member_cursors[prefix] + 1) % counts[prefix];
            }
        }
        self.snapshot_prefix_cursor = (self.snapshot_prefix_cursor + 1) % prefix_order.len().max(1);
        output
    }

    /// Lowest-coverage first-byte ID prefixes from the full eligible reservoir,
    /// not from its smaller published sample. Always contains at least one item.
    pub(crate) fn sparse_prefixes(&self) -> &[u8] {
        &self.sparse_prefixes
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
