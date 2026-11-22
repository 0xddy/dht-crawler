use crate::budget::RateBucket;
use ahash::AHashMap;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

const SOURCE_EXPIRY_TTL: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
struct SourceResponseWindow {
    started_at: Instant,
    last_seen: Instant,
    count: u32,
}

pub(super) struct WorkerResponseLimiter {
    regular_packets: RateBucket,
    regular_bytes: RateBucket,
    priority_packets: RateBucket,
    priority_bytes: RateBucket,
    per_source_rate: u32,
    sources: AHashMap<SocketAddr, SourceResponseWindow>,
    source_expiry: VecDeque<(Instant, SocketAddr)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ResponsePermit {
    Regular,
    PriorityReserve,
    Rejected,
}

fn reserve_quota(total: u32) -> u32 {
    if total == 0 {
        0
    } else {
        total.div_ceil(10).min(total)
    }
}

impl WorkerResponseLimiter {
    pub(super) fn new(
        packet_rate: u32,
        byte_rate: u64,
        per_source_rate: u32,
        now: Instant,
    ) -> Self {
        let byte_rate = byte_rate.min(u32::MAX as u64) as u32;
        let priority_packet_rate = reserve_quota(packet_rate);
        let priority_byte_rate = reserve_quota(byte_rate);
        let packet_rate = packet_rate.saturating_sub(priority_packet_rate);
        let byte_rate = byte_rate.saturating_sub(priority_byte_rate);
        Self {
            regular_packets: RateBucket::per_second(
                packet_rate,
                packet_rate.div_ceil(5).max(1),
                true,
                now,
            ),
            regular_bytes: RateBucket::per_second(
                byte_rate,
                byte_rate.div_ceil(5).max(512),
                true,
                now,
            ),
            priority_packets: RateBucket::per_second(
                priority_packet_rate,
                priority_packet_rate.div_ceil(5).max(1),
                true,
                now,
            ),
            priority_bytes: RateBucket::per_second(
                priority_byte_rate,
                priority_byte_rate.div_ceil(5).max(512),
                true,
                now,
            ),
            per_source_rate,
            sources: AHashMap::new(),
            source_expiry: VecDeque::new(),
        }
    }

    pub(super) fn acquire(
        &mut self,
        addr: SocketAddr,
        encoded_len: usize,
        is_priority: bool,
        now: Instant,
    ) -> ResponsePermit {
        self.expire_sources(now);
        if Self::take_budget(
            &mut self.regular_packets,
            &mut self.regular_bytes,
            encoded_len,
            now,
        ) {
            if self.acquire_source_slot(addr, now) {
                return ResponsePermit::Regular;
            }
            Self::refund_budget(
                &mut self.regular_packets,
                &mut self.regular_bytes,
                encoded_len,
            );
            return ResponsePermit::Rejected;
        }

        if is_priority
            && Self::take_budget(
                &mut self.priority_packets,
                &mut self.priority_bytes,
                encoded_len,
                now,
            )
        {
            if self.acquire_source_slot(addr, now) {
                return ResponsePermit::PriorityReserve;
            }
            Self::refund_budget(
                &mut self.priority_packets,
                &mut self.priority_bytes,
                encoded_len,
            );
        }

        ResponsePermit::Rejected
    }

    fn take_budget(
        packets: &mut RateBucket,
        bytes: &mut RateBucket,
        encoded_len: usize,
        now: Instant,
    ) -> bool {
        if !packets.try_take_one(now) {
            return false;
        }
        if !bytes.try_take_exact(encoded_len, now) {
            packets.refund_one();
            return false;
        }
        true
    }

    fn refund_budget(packets: &mut RateBucket, bytes: &mut RateBucket, encoded_len: usize) {
        packets.refund_one();
        bytes.refund(encoded_len);
    }

    fn acquire_source_slot(&mut self, addr: SocketAddr, now: Instant) -> bool {
        if self.per_source_rate == 0 {
            return false;
        }

        let mut inserted = false;
        let entry = self.sources.entry(addr).or_insert_with(|| {
            inserted = true;
            SourceResponseWindow {
                started_at: now,
                last_seen: now,
                count: 0,
            }
        });
        if now
            .checked_duration_since(entry.started_at)
            .unwrap_or_default()
            >= Duration::from_secs(1)
        {
            entry.started_at = now;
            entry.count = 0;
        }
        if entry.count >= self.per_source_rate {
            return false;
        }
        entry.count += 1;
        entry.last_seen = now;
        if inserted {
            self.source_expiry
                .push_back((now + SOURCE_EXPIRY_TTL, addr));
        }
        true
    }

    fn expire_sources(&mut self, now: Instant) {
        while let Some((deadline, addr)) = self.source_expiry.front().copied() {
            if deadline > now {
                break;
            }
            self.source_expiry.pop_front();
            if let Some(entry) = self.sources.get(&addr) {
                let expire_at = entry.last_seen + SOURCE_EXPIRY_TTL;
                if expire_at <= now {
                    self.sources.remove(&addr);
                } else {
                    self.source_expiry.push_back((expire_at, addr));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_limiter_enforces_packet_byte_and_source_limits() {
        let start = Instant::now();
        let addr: SocketAddr = "8.8.8.8:6881".parse().unwrap();
        let mut limiter = WorkerResponseLimiter::new(2, 200, 1, start);
        assert_eq!(
            limiter.acquire(addr, 100, false, start),
            ResponsePermit::Regular
        );
        assert_eq!(
            limiter.acquire(addr, 100, false, start),
            ResponsePermit::Rejected
        );
        assert_eq!(
            limiter.acquire(addr, 100, false, start + Duration::from_secs(1)),
            ResponsePermit::Regular
        );
        assert_eq!(
            limiter.acquire(
                "1.1.1.1:6881".parse().unwrap(),
                513,
                false,
                start + Duration::from_secs(2)
            ),
            ResponsePermit::Rejected
        );
    }

    #[test]
    fn ping_and_get_peers_can_use_the_priority_reserve() {
        let start = Instant::now();
        let mut limiter = WorkerResponseLimiter::new(10, 1_000, 100, start);
        let first: SocketAddr = "8.8.8.8:6881".parse().unwrap();
        let second: SocketAddr = "1.1.1.1:6881".parse().unwrap();
        let priority: SocketAddr = "9.9.9.9:6881".parse().unwrap();

        assert_eq!(
            limiter.acquire(first, 50, false, start),
            ResponsePermit::Regular
        );
        assert_eq!(
            limiter.acquire(second, 50, false, start),
            ResponsePermit::Regular
        );
        assert_eq!(
            limiter.acquire(priority, 50, false, start),
            ResponsePermit::Rejected
        );
        assert_eq!(
            limiter.acquire(priority, 50, true, start),
            ResponsePermit::PriorityReserve
        );
        assert_eq!(
            limiter.acquire("4.4.4.4:6881".parse().unwrap(), 50, true, start),
            ResponsePermit::Rejected
        );
    }

    #[test]
    fn response_limiter_coalesces_source_expiry_entries_within_one_second() {
        let start = Instant::now();
        let addr: SocketAddr = "8.8.8.8:6881".parse().unwrap();
        let mut limiter = WorkerResponseLimiter::new(1_000, 100_000, 100, start);
        for offset_ms in [0, 200, 400, 900] {
            assert_eq!(
                limiter.acquire(addr, 50, false, start + Duration::from_millis(offset_ms)),
                ResponsePermit::Regular
            );
        }
        assert_eq!(limiter.source_expiry.len(), 1);
        assert_eq!(limiter.sources.len(), 1);

        // At 5s, last_seen was 0.9s so it is deferred once to 5.9s without leaking.
        limiter.expire_sources(start + Duration::from_secs(5));
        assert_eq!(limiter.sources.len(), 1);
        assert_eq!(limiter.source_expiry.len(), 1);

        limiter.expire_sources(start + Duration::from_secs(6));
        assert!(limiter.sources.is_empty());
        assert!(limiter.source_expiry.is_empty());
    }
}
