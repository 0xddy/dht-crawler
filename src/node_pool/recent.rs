use ahash::AHashMap;
use std::net::SocketAddr;
use std::time::Instant;

#[derive(Clone, Copy)]
pub(super) struct RecentProbe {
    pub(super) expires_at: Instant,
    pub(super) heap_index: usize,
}

/// One map entry and one heap entry per endpoint. Repeated probes only refresh
/// the map deadline; the heap deadline is lazily advanced when it becomes due.
pub(super) struct RecentProbes {
    pub(super) entries: AHashMap<SocketAddr, RecentProbe>,
    pub(super) expiry: Vec<(Instant, SocketAddr)>,
    pub(super) capacity: usize,
}

impl RecentProbes {
    pub(super) fn new(capacity: usize) -> Self {
        Self {
            entries: AHashMap::new(),
            expiry: Vec::new(),
            capacity: capacity.max(1),
        }
    }

    pub(super) fn contains(&self, addr: &SocketAddr, now: Instant) -> bool {
        self.entries
            .get(addr)
            .is_some_and(|entry| entry.expires_at > now)
    }

    pub(super) fn record(&mut self, addr: SocketAddr, expires_at: Instant) {
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

    pub(super) fn remove(&mut self, addr: &SocketAddr) {
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

    pub(super) fn has_due(&self, now: Instant) -> bool {
        self.expiry.first().is_some_and(|entry| entry.0 <= now)
    }

    pub(super) fn expire(&mut self, now: Instant) -> usize {
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
