use crate::types::CrawlOptions;
use std::time::Duration;

#[derive(Debug, Clone)]
pub(crate) struct ResolvedCrawlConfig {
    pub(crate) max_find_node_rate_per_sec: u32,
    pub(crate) burst: u32,
    pub(crate) max_in_flight: usize,
    pub(crate) request_timeout: Duration,
    pub(crate) max_new_destinations_per_minute: u32,
    pub(crate) max_response_rate_per_sec: u32,
    pub(crate) max_response_bytes_per_sec: u64,
    pub(crate) max_response_rate_per_source: u32,
    pub(crate) metadata_pressure_floor_percent: u8,

    pub(crate) pool_capacity: usize,
    pub(crate) max_replacements_per_minute: u32,
    pub(crate) recent_probe_ttl: Duration,
    pub(crate) responsive_capacity: usize,
    pub(crate) responsive_ttl: Duration,
    pub(crate) low_watermark: usize,
    pub(crate) max_in_flight_per_subnet: usize,

    pub(crate) bootstrap_nodes: Vec<String>,
    pub(crate) bootstrap_interval: Duration,
    pub(crate) bootstrap_max_nodes_per_round: usize,
    pub(crate) bootstrap_backoff_base: Duration,
    pub(crate) bootstrap_backoff_max: Duration,

    pub(crate) random_walk_percent: u8,
    pub(crate) sparse_bucket_percent: u8,
    pub(crate) neighbor_sender_id: bool,

    pub(crate) priority_event_channel_capacity: usize,
    pub(crate) discovery_event_channel_capacity: usize,
    pub(crate) event_batch_limit: usize,
    pub(crate) node_batch_limit: usize,
    pub(crate) routing_snapshot_size: usize,
    pub(crate) snapshot_refresh: Duration,
}

impl ResolvedCrawlConfig {
    pub(crate) fn from_options(options: &CrawlOptions) -> Self {
        let capacity = options.pool.capacity.max(1);
        Self {
            max_find_node_rate_per_sec: options.rate_limit.max_find_node_rate_per_sec,
            burst: if options.rate_limit.max_find_node_rate_per_sec == 0 {
                0
            } else {
                options.rate_limit.burst.max(1)
            },
            max_in_flight: options.rate_limit.max_in_flight.max(1),
            request_timeout: Duration::from_secs(options.rate_limit.request_timeout_secs.max(1)),
            max_new_destinations_per_minute: options.rate_limit.max_new_destinations_per_minute,
            max_response_rate_per_sec: options.rate_limit.max_response_rate_per_sec,
            max_response_bytes_per_sec: options.rate_limit.max_response_bytes_per_sec,
            max_response_rate_per_source: options.rate_limit.max_response_rate_per_source,
            metadata_pressure_floor_percent: options
                .rate_limit
                .metadata_pressure_floor_percent
                .min(100),

            pool_capacity: capacity,
            max_replacements_per_minute: options.rate_limit.max_replacements_per_minute,
            recent_probe_ttl: Duration::from_secs(options.pool.recent_probe_ttl_secs.max(1)),
            responsive_capacity: options.pool.responsive_capacity.max(1),
            responsive_ttl: Duration::from_secs(options.pool.responsive_ttl_secs.max(1)),
            low_watermark: options.pool.low_watermark.min(capacity),
            max_in_flight_per_subnet: options.rate_limit.max_in_flight_per_subnet.max(1),

            bootstrap_nodes: if options.bootstrap.nodes.is_empty() {
                crate::types::BootstrapOptions::default().nodes
            } else {
                options.bootstrap.nodes.clone()
            },
            bootstrap_interval: Duration::from_secs(options.bootstrap.interval_secs),
            bootstrap_max_nodes_per_round: options.bootstrap.max_nodes_per_round,
            bootstrap_backoff_base: Duration::from_secs(
                options.bootstrap.source_backoff_base_secs.max(1),
            ),
            bootstrap_backoff_max: Duration::from_secs(
                options.bootstrap.source_backoff_max_secs.max(1),
            ),

            random_walk_percent: options.target.random_walk_percent.min(100),
            sparse_bucket_percent: options.target.sparse_bucket_percent.min(100),
            neighbor_sender_id: options.target.neighbor_sender_id,

            priority_event_channel_capacity: options
                .scheduler
                .priority_event_channel_capacity
                .max(1),
            discovery_event_channel_capacity: options
                .scheduler
                .discovery_event_channel_capacity
                .max(1),
            event_batch_limit: options.scheduler.event_batch_limit.max(1),
            node_batch_limit: options.scheduler.node_batch_limit.max(1),
            routing_snapshot_size: options.scheduler.routing_snapshot_size.max(1),
            snapshot_refresh: Duration::from_millis(
                options.scheduler.snapshot_refresh_millis.max(100),
            ),
        }
    }

    pub(crate) fn rate_for_metadata_pressure(&self, pressure: f64) -> u32 {
        let max = f64::from(self.max_find_node_rate_per_sec);
        if pressure < 0.80 {
            return self.max_find_node_rate_per_sec;
        }
        let floor = max * f64::from(self.metadata_pressure_floor_percent) / 100.0;
        if pressure >= 0.95 {
            return floor.round() as u32;
        }
        let progress = ((pressure - 0.80) / 0.15).clamp(0.0, 1.0);
        (max - (max - floor) * progress).round() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_pool_and_rate_limit() {
        let mut options = CrawlOptions::default();
        options.rate_limit.max_find_node_rate_per_sec = 200;
        options.rate_limit.metadata_pressure_floor_percent = 25;
        options.pool.capacity = 42;
        let resolved = ResolvedCrawlConfig::from_options(&options);

        assert_eq!(resolved.pool_capacity, 42);
        assert_eq!(resolved.rate_for_metadata_pressure(0.79), 200);
        assert_eq!(resolved.rate_for_metadata_pressure(0.95), 50);
        assert_eq!(resolved.rate_for_metadata_pressure(1.0), 50);
    }
}
