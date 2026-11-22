mod recent;
mod responsive;
#[cfg(test)]
mod tests;

use crate::addr::is_valid_node_addr;
use crate::budget::RateBucket;
use crate::types::NodeTuple;
use ahash::AHashSet;
use recent::RecentProbes;
pub(crate) use responsive::{ResponsiveReservoir, SubnetKey};
use std::collections::VecDeque;
use std::net::SocketAddr;
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
        if self.warmed
            && self.queue.len() >= self.capacity
            && !self.replacement_budget.try_take_one(now)
        {
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
