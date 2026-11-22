use ahash::{AHashMap, AHasher};
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const PEER_FAILURE_CACHE_SHARDS: usize = 64;
const MAX_EXPIRE_PER_OP: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeerFailureReason {
    Timeout,
    ConnectFailed,
    MetadataFailed,
}

impl PeerFailureReason {
    #[cfg(feature = "metrics")]
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::ConnectFailed => "connect_failed",
            Self::MetadataFailed => "metadata_failed",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PeerFailureEntry {
    expires_at: Instant,
    reason: PeerFailureReason,
}

/// A reachable peer can serve other torrents even when one handshake or download fails.
/// Both scopes share a single configured capacity and TTL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum PeerFailureKey {
    Endpoint(SocketAddr),
    Metadata([u8; 20], SocketAddr),
}

#[derive(Default)]
struct PeerFailureCacheInner {
    entries: AHashMap<PeerFailureKey, PeerFailureEntry>,
    expiry: VecDeque<(Instant, PeerFailureKey)>,
}

struct PeerFailureShard {
    inner: Mutex<PeerFailureCacheInner>,
    capacity: usize,
}

pub(crate) struct PeerFailureCache {
    shards: Vec<PeerFailureShard>,
    total_entries: AtomicUsize,
    capacity: usize,
    ttl: Duration,
}

impl PeerFailureCache {
    pub(crate) fn new(capacity: usize, ttl: Duration) -> Self {
        if capacity == 0 || ttl.is_zero() {
            return Self {
                shards: Vec::new(),
                total_entries: AtomicUsize::new(0),
                capacity,
                ttl,
            };
        }
        let num_shards = if capacity < PEER_FAILURE_CACHE_SHARDS {
            1
        } else {
            PEER_FAILURE_CACHE_SHARDS
        };
        let shard_cap = capacity.div_ceil(num_shards);
        let prealloc = shard_cap.min(256);
        let shards = (0..num_shards)
            .map(|_| PeerFailureShard {
                inner: Mutex::new(PeerFailureCacheInner {
                    entries: AHashMap::with_capacity(prealloc),
                    expiry: VecDeque::with_capacity(prealloc),
                }),
                capacity: shard_cap,
            })
            .collect();
        Self {
            shards,
            total_entries: AtomicUsize::new(0),
            capacity,
            ttl,
        }
    }

    fn shard_index(&self, key: &PeerFailureKey) -> usize {
        if self.shards.len() <= 1 {
            return 0;
        }
        let mut hasher = AHasher::default();
        key.hash(&mut hasher);
        (hasher.finish() as usize) % self.shards.len()
    }

    fn update_total(&self, before: usize, after: usize) -> usize {
        if after > before {
            self.total_entries
                .fetch_add(after - before, Ordering::Relaxed)
                + (after - before)
        } else if before > after {
            self.total_entries
                .fetch_sub(before - after, Ordering::Relaxed)
                .saturating_sub(before - after)
        } else {
            self.total_entries.load(Ordering::Relaxed)
        }
    }

    pub(crate) fn get(&self, key: PeerFailureKey, now: Instant) -> (Option<PeerFailureReason>, usize) {
        if self.capacity == 0 || self.ttl.is_zero() || self.shards.is_empty() {
            return (None, 0);
        }
        let shard = &self.shards[self.shard_index(&key)];
        let mut inner = shard
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = inner.entries.len();
        Self::expire(&mut inner, now);
        let reason = match inner.entries.get(&key).copied() {
            Some(entry) if entry.expires_at > now => Some(entry.reason),
            Some(_) => {
                inner.entries.remove(&key);
                None
            }
            None => None,
        };
        let total = self.update_total(before, inner.entries.len());
        (reason, total)
    }

    pub(crate) fn insert(&self, addr: PeerFailureKey, reason: PeerFailureReason, now: Instant) -> usize {
        if self.capacity == 0 || self.ttl.is_zero() || self.shards.is_empty() {
            return 0;
        }
        let shard = &self.shards[self.shard_index(&addr)];
        let mut inner = shard
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = inner.entries.len();
        Self::expire(&mut inner, now);
        while inner.entries.len() >= shard.capacity && !inner.entries.contains_key(&addr) {
            let Some((expires_at, oldest_addr)) = inner.expiry.pop_front() else {
                break;
            };
            if inner
                .entries
                .get(&oldest_addr)
                .is_some_and(|entry| entry.expires_at == expires_at)
            {
                inner.entries.remove(&oldest_addr);
            }
        }

        let expires_at = now + self.ttl;
        inner
            .entries
            .insert(addr, PeerFailureEntry { expires_at, reason });
        inner.expiry.push_back((expires_at, addr));
        self.update_total(before, inner.entries.len())
    }

    pub(crate) fn remove(&self, addr: &PeerFailureKey, now: Instant) -> usize {
        if self.capacity == 0 || self.ttl.is_zero() || self.shards.is_empty() {
            return 0;
        }
        let shard = &self.shards[self.shard_index(addr)];
        let mut inner = shard
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = inner.entries.len();
        Self::expire(&mut inner, now);
        inner.entries.remove(addr);
        self.update_total(before, inner.entries.len())
    }

    fn expire(inner: &mut PeerFailureCacheInner, now: Instant) {
        for _ in 0..MAX_EXPIRE_PER_OP {
            let Some((expires_at, addr)) = inner.expiry.front().copied() else {
                break;
            };
            if expires_at > now {
                break;
            }
            inner.expiry.pop_front();
            if inner
                .entries
                .get(&addr)
                .is_some_and(|entry| entry.expires_at == expires_at)
            {
                inner.entries.remove(&addr);
            }
        }
    }
}
