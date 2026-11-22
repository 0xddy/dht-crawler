use std::sync::atomic::{AtomicU64, Ordering};

pub const METADATA_QUEUE_WAIT_BUCKETS_MS: [u64; 8] = [10, 50, 100, 250, 500, 1_000, 2_000, 5_000];
pub const METADATA_FETCH_BUCKETS_MS: [u64; 7] = [250, 500, 1_000, 2_000, 4_000, 6_000, 10_000];
pub const METADATA_SIZE_BUCKETS_BYTES: [u64; 8] = [
    16 * 1024,
    32 * 1024,
    64 * 1024,
    128 * 1024,
    256 * 1024,
    512 * 1024,
    1024 * 1024,
    10 * 1024 * 1024,
];

#[derive(Debug, Clone, PartialEq, Eq)]
/// Snapshot of a non-cumulative fixed-bucket histogram.
pub struct FixedHistogramSnapshot {
    /// Inclusive upper bound for each bucket.
    pub bounds: Vec<u64>,
    /// Per-bucket counts; values are not cumulative.
    pub counts: Vec<u64>,
    /// Values larger than the final bound.
    pub overflow: u64,
    /// Total observations including overflow.
    pub count: u64,
    /// Wrapping sum of all observed values.
    pub sum: u64,
}

impl FixedHistogramSnapshot {
    /// Returns the inclusive bucket bound containing the requested approximate percentile.
    ///
    /// `percentile` is clamped to `0.0..=1.0`. Overflow observations return the final finite
    /// bound because the histogram intentionally stores no dynamic maximum.
    pub fn percentile(&self, percentile: f64) -> Option<u64> {
        if self.count == 0 {
            return None;
        }
        let rank = ((self.count as f64 * percentile.clamp(0.0, 1.0)).ceil() as u64).max(1);
        let mut cumulative = 0u64;
        for (bound, count) in self.bounds.iter().zip(&self.counts) {
            cumulative = cumulative.saturating_add(*count);
            if cumulative >= rank {
                return Some(*bound);
            }
        }
        self.bounds.last().copied()
    }
}

pub(super) struct AtomicFixedHistogram<const N: usize> {
    bounds: [u64; N],
    counts: [AtomicU64; N],
    overflow: AtomicU64,
    count: AtomicU64,
    sum: AtomicU64,
}

impl<const N: usize> AtomicFixedHistogram<N> {
    pub(super) fn new(bounds: [u64; N]) -> Self {
        Self {
            bounds,
            counts: std::array::from_fn(|_| AtomicU64::new(0)),
            overflow: AtomicU64::new(0),
            count: AtomicU64::new(0),
            sum: AtomicU64::new(0),
        }
    }

    pub(super) fn record(&self, value: u64) {
        if let Some(index) = self.bounds.iter().position(|bound| value <= *bound) {
            self.counts[index].fetch_add(1, Ordering::Relaxed);
        } else {
            self.overflow.fetch_add(1, Ordering::Relaxed);
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum.fetch_add(value, Ordering::Relaxed);
    }

    pub(super) fn snapshot(&self) -> FixedHistogramSnapshot {
        FixedHistogramSnapshot {
            bounds: self.bounds.to_vec(),
            counts: self
                .counts
                .iter()
                .map(|count| count.load(Ordering::Relaxed))
                .collect(),
            overflow: self.overflow.load(Ordering::Relaxed),
            count: self.count.load(Ordering::Relaxed),
            sum: self.sum.load(Ordering::Relaxed),
        }
    }
}

impl<const N: usize> Default for AtomicFixedHistogram<N> {
    fn default() -> Self {
        Self::new([0; N])
    }
}
