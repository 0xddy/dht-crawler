use super::MAX_METADATA_CANDIDATES_PER_HASH;
use super::retry::RetryRecord;
use crate::server::{HashDiscovered, HashSource};
use crate::types::NodeTuple;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub(super) const HASH_QUEUE_TTL: Duration = Duration::from_secs(60);
const PEER_COALESCE_WINDOW: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Copy)]
pub(super) struct PeerCandidate {
    pub(super) addr: SocketAddr,
    pub(super) discovered_at: Instant,
}

#[derive(Debug, Clone)]
pub(super) struct QueuedHash {
    pub(super) info_hash: String,
    pub(super) peers: Vec<PeerCandidate>,
    pub(super) queued_at: Instant,
    pub(super) ready_at: Instant,
    pub(super) latest_at: Instant,
    pub(super) order_key: (Instant, u64),
    pub(super) source_nodes: Vec<NodeTuple>,
    pub(super) source: HashSource,
    pub(super) expires_at: Instant,
    pub(super) is_retry: bool,
    pub(super) ready: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QueuePushKind {
    Inserted,
    Updated,
    EvictedOldest,
    EvictedRetry,
    Stale,
}

#[derive(Debug)]
pub(super) struct InFlightState {
    pub(super) peer_tx: mpsc::Sender<PeerCandidate>,
    pub(super) scheduled_peers: HashSet<SocketAddr>,
    pub(super) entry: QueuedHash,
}

#[derive(Debug)]
pub(super) struct PendingHashQueue {
    capacity: usize,
    ttl: Duration,
    pub(super) entries: HashMap<String, QueuedHash>,
    // Each hash has exactly one expiry and scheduling entry. Refresh never appends tombstones.
    pub(super) expiry: BTreeMap<(Instant, u64), String>,
    pub(super) waiting: BTreeMap<(Instant, u64), String>,
    pub(super) ready_fresh: BTreeMap<(Instant, u64), String>,
    pub(super) ready_retry: BTreeMap<(Instant, u64), String>,
    pub(super) retry_expiry: BTreeMap<(Instant, u64), String>,
    sequence: u64,
    fresh_dispatches: u8,
}

pub(super) fn merge_candidate(entry: &mut QueuedHash, event: &HashDiscovered) -> bool {
    let mut new_peer = false;
    if let Some(addr) = event.peer_addr {
        new_peer = !entry.peers.iter().any(|peer| peer.addr == addr);
        let discovered_at = entry
            .peers
            .iter()
            .find(|peer| peer.addr == addr)
            .map_or(event.discovered_at, |peer| {
                peer.discovered_at.max(event.discovered_at)
            });
        entry.peers.retain(|peer| peer.addr != addr);
        entry.peers.push(PeerCandidate {
            addr,
            discovered_at,
        });
        entry
            .peers
            .sort_unstable_by(|a, b| b.discovered_at.cmp(&a.discovered_at));
        entry.peers.truncate(MAX_METADATA_CANDIDATES_PER_HASH);
    }
    if let Some(node) = &event.source_node {
        entry.source_nodes.retain(|old| old.addr != node.addr);
        entry.source_nodes.insert(0, *node);
        entry.source_nodes.truncate(8);
    }
    if matches!(event.source, HashSource::Announce) {
        entry.source = HashSource::Announce;
    }
    entry.latest_at = entry.latest_at.max(event.discovered_at);
    new_peer
}

impl PendingHashQueue {
    pub(super) fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            capacity: capacity.max(1),
            ttl,
            entries: HashMap::new(),
            expiry: BTreeMap::new(),
            waiting: BTreeMap::new(),
            ready_fresh: BTreeMap::new(),
            ready_retry: BTreeMap::new(),
            retry_expiry: BTreeMap::new(),
            sequence: 0,
            fresh_dispatches: 0,
        }
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(super) fn pressure_len(&self) -> usize {
        self.entries.len().saturating_sub(self.retry_expiry.len()) + self.ready_retry.len()
    }

    #[cfg(test)]
    pub(super) fn contains(&self, hash: &str) -> bool {
        self.entries.contains_key(hash)
    }

    fn insert(&mut self, mut entry: QueuedHash) {
        self.sequence = self.sequence.wrapping_add(1);
        entry.order_key = (entry.latest_at, self.sequence);
        self.expiry
            .insert((entry.expires_at, self.sequence), entry.info_hash.clone());
        if entry.is_retry {
            self.retry_expiry
                .insert((entry.expires_at, self.sequence), entry.info_hash.clone());
        }
        if entry.ready {
            if entry.is_retry {
                self.ready_retry
                    .insert((entry.ready_at, self.sequence), entry.info_hash.clone());
            } else {
                self.ready_fresh
                    .insert(entry.order_key, entry.info_hash.clone());
            }
        } else {
            self.waiting
                .insert((entry.ready_at, self.sequence), entry.info_hash.clone());
        }
        self.entries.insert(entry.info_hash.clone(), entry);
    }

    pub(super) fn push(&mut self, event: HashDiscovered, now: Instant) -> QueuePushKind {
        if now
            .checked_duration_since(event.discovered_at)
            .unwrap_or_default()
            > self.ttl
        {
            return QueuePushKind::Stale;
        }
        if let Some(mut entry) = self.remove(&event.info_hash) {
            merge_candidate(&mut entry, &event);
            if !entry.is_retry {
                entry.expires_at = entry.latest_at + self.ttl;
            }
            self.insert(entry);
            return QueuePushKind::Updated;
        }
        let mut result = QueuePushKind::Inserted;
        if self.entries.len() >= self.capacity {
            let Some((_, oldest_hash)) = self
                .retry_expiry
                .first_key_value()
                .or_else(|| self.expiry.first_key_value())
            else {
                return QueuePushKind::Stale;
            };
            let oldest_hash = oldest_hash.clone();
            let oldest = &self.entries[&oldest_hash];
            if !oldest.is_retry && event.discovered_at <= oldest.latest_at {
                return QueuePushKind::Stale;
            }
            result = if oldest.is_retry {
                QueuePushKind::EvictedRetry
            } else {
                QueuePushKind::EvictedOldest
            };
            self.remove(&oldest_hash);
        }
        let mut entry = QueuedHash {
            info_hash: event.info_hash.clone(),
            peers: Vec::new(),
            source_nodes: Vec::new(),
            source: event.source,
            queued_at: now,
            ready_at: now + PEER_COALESCE_WINDOW,
            latest_at: event.discovered_at,
            order_key: (now, 0),
            expires_at: event.discovered_at + self.ttl,
            is_retry: false,
            ready: false,
        };
        merge_candidate(&mut entry, &event);
        self.insert(entry);
        result
    }

    pub(super) fn remove(&mut self, hash: &str) -> Option<QueuedHash> {
        let entry = self.entries.remove(hash)?;
        let sequence = entry.order_key.1;
        self.expiry.remove(&(entry.expires_at, sequence));
        self.retry_expiry.remove(&(entry.expires_at, sequence));
        self.waiting.remove(&(entry.ready_at, sequence));
        self.ready_fresh.remove(&entry.order_key);
        self.ready_retry.remove(&(entry.ready_at, sequence));
        Some(entry)
    }

    pub(super) fn can_defer(&self, hash: &str) -> bool {
        let retry_limit = (self.capacity / 4).max(1);
        if let Some(entry) = self.entries.get(hash) {
            entry.is_retry || self.retry_expiry.len() < retry_limit
        } else {
            self.entries.len() < self.capacity && self.retry_expiry.len() < retry_limit
        }
    }

    pub(super) fn defer(&mut self, mut entry: QueuedHash, record: RetryRecord) -> bool {
        self.remove(&entry.info_hash);
        if self.entries.len() >= self.capacity
            || self.retry_expiry.len() >= (self.capacity / 4).max(1)
        {
            return false;
        }
        entry.ready_at = record.next_due;
        entry.expires_at = record.expires_at;
        entry.is_retry = true;
        entry.ready = false;
        self.insert(entry);
        true
    }

    pub(super) fn promote_ready(&mut self, now: Instant) {
        while let Some((&key, _)) = self.waiting.first_key_value() {
            if key.0 > now {
                break;
            }
            let (_, hash) = self.waiting.pop_first().unwrap();
            let entry = self.entries.get_mut(&hash).expect("indexed queued hash");
            entry.ready = true;
            if entry.is_retry {
                self.ready_retry.insert(key, hash);
            } else {
                self.ready_fresh.insert(entry.order_key, hash);
            }
        }
    }

    pub(super) fn pop_newest_ready(
        &mut self,
        now: Instant,
        _in_flight: &HashMap<String, InFlightState>,
    ) -> Option<QueuedHash> {
        self.expire(now);
        self.promote_ready(now);
        // At most three fresh jobs before one due retry. Either class borrows idle capacity.
        let use_retry = !self.ready_retry.is_empty()
            && (self.fresh_dispatches >= 3 || self.ready_fresh.is_empty());
        let hash = if use_retry {
            self.fresh_dispatches = 0;
            self.ready_retry.first_key_value()?.1.clone()
        } else {
            self.fresh_dispatches = self.fresh_dispatches.saturating_add(1);
            self.ready_fresh.last_key_value()?.1.clone()
        };
        self.remove(&hash)
    }

    pub(super) fn expire(&mut self, now: Instant) -> usize {
        let mut expired = 0;
        while let Some((&key, hash)) = self.expiry.first_key_value() {
            if key.0 >= now {
                break;
            }
            let hash = hash.clone();
            self.remove(&hash);
            expired += 1;
        }
        expired
    }
}
