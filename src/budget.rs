use std::time::{Duration, Instant};

/// Single-owner token bucket. It deliberately contains no atomics or locks.
pub(crate) struct RateBucket {
    rate_per_sec: f64,
    capacity: f64,
    tokens: f64,
    last_refill: Instant,
}

impl RateBucket {
    pub(crate) fn per_second(
        rate_per_sec: u32,
        burst: u32,
        initially_full: bool,
        now: Instant,
    ) -> Self {
        Self::new(
            f64::from(rate_per_sec),
            f64::from(burst),
            initially_full,
            now,
        )
    }

    pub(crate) fn per_minute(
        rate_per_minute: u32,
        burst: u32,
        initially_full: bool,
        now: Instant,
    ) -> Self {
        Self::new(
            f64::from(rate_per_minute) / 60.0,
            f64::from(burst),
            initially_full,
            now,
        )
    }

    fn new(rate_per_sec: f64, capacity: f64, initially_full: bool, now: Instant) -> Self {
        let capacity = if rate_per_sec <= 0.0 {
            0.0
        } else {
            capacity.max(1.0)
        };
        Self {
            rate_per_sec,
            capacity,
            tokens: if initially_full { capacity } else { 0.0 },
            last_refill: now,
        }
    }

    pub(crate) fn set_per_second_rate(&mut self, rate_per_sec: u32, now: Instant) {
        self.refill(now);
        self.rate_per_sec = f64::from(rate_per_sec);
        if self.rate_per_sec <= 0.0 {
            self.tokens = 0.0;
        } else {
            self.tokens = self.tokens.min(self.capacity);
        }
    }

    pub(crate) fn try_take_one(&mut self, now: Instant) -> bool {
        self.try_take_exact(1, now)
    }

    pub(crate) fn try_take_exact(&mut self, count: usize, now: Instant) -> bool {
        if count == 0 {
            return true;
        }
        if self.rate_per_sec <= 0.0 || self.capacity <= 0.0 {
            return false;
        }
        self.refill(now);
        if self.tokens < count as f64 {
            return false;
        }
        self.tokens -= count as f64;
        true
    }

    pub(crate) fn try_take(&mut self, max: usize, now: Instant) -> usize {
        if max == 0 || self.rate_per_sec <= 0.0 || self.capacity <= 0.0 {
            return 0;
        }
        self.refill(now);
        let taken = (self.tokens.floor() as usize).min(max);
        self.tokens -= taken as f64;
        taken
    }

    pub(crate) fn refund_one(&mut self) {
        self.refund(1);
    }

    pub(crate) fn refund(&mut self, count: usize) {
        self.tokens = (self.tokens + count as f64).min(self.capacity);
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now
            .checked_duration_since(self.last_refill)
            .unwrap_or(Duration::ZERO)
            .as_secs_f64();
        if elapsed > 0.0 {
            self.tokens = (self.tokens + elapsed * self.rate_per_sec).min(self.capacity);
            self.last_refill = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_bucket_is_smooth_and_bounded() {
        let start = Instant::now();
        let mut bucket = RateBucket::per_second(100, 20, false, start);
        assert_eq!(bucket.try_take(100, start), 0);
        assert_eq!(bucket.try_take(100, start + Duration::from_millis(100)), 10);
        assert_eq!(bucket.try_take(100, start + Duration::from_secs(10)), 20);
    }

    #[test]
    fn minute_bucket_refills_fractionally() {
        let start = Instant::now();
        let mut bucket = RateBucket::per_minute(600, 10, false, start);
        assert_eq!(bucket.try_take(100, start + Duration::from_millis(500)), 5);
    }
}
