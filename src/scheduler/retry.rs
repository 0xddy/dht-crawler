use super::MAX_METADATA_CANDIDATES_PER_HASH;
use super::queue::QueuedHash;
use crate::server::HashDiscovered;
use crate::types::MetadataOptions;
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

pub(super) const MAX_RETRY_LIFETIME: Duration = Duration::from_secs(7 * 24 * 60 * 60);
pub(super) const MAX_DEFER_RECHECKS: u8 = 6;
const MAX_DEFER_BACKOFF: Duration = Duration::from_secs(60);

fn retry_deadline(now: Instant, delay: Duration) -> Instant {
    now.checked_add(delay.min(MAX_RETRY_LIFETIME))
        .unwrap_or(now)
}

#[derive(Debug, Clone, Copy)]
pub(super) struct RetryRecord {
    pub(super) failures: usize,
    pub(super) next_due: Instant,
    pub(super) expires_at: Instant,
    pub(super) extra_attempts: usize,
    pub(super) defers: u8,
}

impl RetryRecord {
    pub(super) fn attempt_limit(self, policy: &MetadataOptions) -> usize {
        policy
            .retry_max_attempts
            .saturating_add(self.extra_attempts)
    }
}

pub(super) struct RetryState {
    record: RetryRecord,
    peers: Vec<SocketAddr>,
    sources: Vec<SocketAddr>,
}

impl RetryState {
    fn observe(&mut self, peer: Option<SocketAddr>, source: Option<SocketAddr>) -> bool {
        let mut novel = false;
        for (candidate, seen, capacity) in [
            (peer, &mut self.peers, MAX_METADATA_CANDIDATES_PER_HASH),
            (source, &mut self.sources, 8),
        ] {
            if let Some(addr) = candidate
                && !seen.contains(&addr)
            {
                novel = true;
                if seen.len() < capacity {
                    seen.push(addr);
                }
            }
        }
        novel
    }
}

#[derive(Default)]
pub(super) struct RetryHistory {
    pub(super) entries: HashMap<String, RetryState>,
    pub(super) expiry: BTreeMap<(Instant, String), ()>,
}

impl RetryHistory {
    pub(super) fn expire(&mut self, now: Instant) {
        while let Some(((at, _), _)) = self.expiry.first_key_value() {
            if *at > now {
                break;
            }
            let ((_, hash), _) = self.expiry.pop_first().unwrap();
            self.entries.remove(&hash);
        }
    }

    pub(super) fn remove(&mut self, hash: &str) {
        if let Some(state) = self.entries.remove(hash) {
            self.expiry
                .remove(&(state.record.expires_at, hash.to_string()));
        }
    }

    pub(super) fn record(
        &mut self,
        hash: &str,
        now: Instant,
        failed: bool,
        delay: Duration,
        policy: &MetadataOptions,
    ) -> Option<RetryRecord> {
        self.expire(now);
        if !self.entries.contains_key(hash) {
            if self.entries.len() >= policy.retry_history_capacity {
                return None;
            }
            let expires_at =
                retry_deadline(now, Duration::from_secs(policy.retry_history_ttl_secs));
            self.entries.insert(
                hash.to_string(),
                RetryState {
                    record: RetryRecord {
                        failures: 0,
                        next_due: now,
                        expires_at,
                        extra_attempts: 0,
                        defers: 0,
                    },
                    peers: Vec::new(),
                    sources: Vec::new(),
                },
            );
            self.expiry.insert((expires_at, hash.to_string()), ());
        }
        let record = &mut self.entries.get_mut(hash).unwrap().record;
        let backoff = if failed {
            record.failures = record.failures.saturating_add(1);
            record.defers = 0;
            let multiplier = 1u64 << record.failures.saturating_sub(1).min(31);
            Duration::from_secs(
                policy
                    .retry_initial_delay_secs
                    .saturating_mul(multiplier)
                    .min(policy.retry_max_delay_secs),
            )
        } else {
            record.defers = record.defers.saturating_add(1);
            if record.defers <= 1 {
                delay
            } else {
                let factor = 1u32 << (record.defers - 1).min(4);
                delay
                    .saturating_mul(factor)
                    .min(delay.max(MAX_DEFER_BACKOFF))
            }
        };
        // A zero-duration application deferral must not become a busy admission loop.
        record.next_due = retry_deadline(now, backoff.max(Duration::from_millis(100)));
        Some(*record)
    }

    pub(super) fn observe_event(
        &mut self,
        event: &HashDiscovered,
        now: Instant,
        policy: &MetadataOptions,
    ) -> Option<RetryRecord> {
        self.expire(now);
        let state = self.entries.get_mut(&event.info_hash)?;
        let novel_peer = event
            .peer_addr
            .is_some_and(|addr| !state.peers.contains(&addr));
        let novel = state.observe(
            event.peer_addr,
            event.source_node.as_ref().map(|node| node.addr),
        );
        if state.record.failures >= state.record.attempt_limit(policy)
            && novel
            && state.record.extra_attempts < 2
            && policy.retry_max_attempts > 0
        {
            state.record.extra_attempts += 1;
            state.record.defers = 0;
            let wake_delay = if novel_peer {
                Duration::from_millis(100)
            } else {
                Duration::from_secs(policy.retry_initial_delay_secs)
                    .max(Duration::from_millis(100))
            };
            state.record.next_due = retry_deadline(now, wake_delay);
        } else if novel_peer
            && state.record.failures < state.record.attempt_limit(policy)
            && state.record.next_due
                > retry_deadline(now, Duration::from_secs(policy.retry_initial_delay_secs))
        {
            state.record.defers = 0;
            state.record.next_due = retry_deadline(now, Duration::from_millis(100));
        }
        Some(state.record)
    }

    pub(super) fn remember(&mut self, entry: &QueuedHash) {
        if let Some(state) = self.entries.get_mut(&entry.info_hash) {
            for peer in &entry.peers {
                state.observe(Some(peer.addr), None);
            }
            for node in &entry.source_nodes {
                state.observe(None, Some(node.addr));
            }
        }
    }
}
