use std::sync::atomic::{AtomicU64, Ordering};

// Fixed names keep the packet path allocation-free and prevent high-cardinality labels.
macro_rules! yield_counters {
    ($($name:ident),+ $(,)?) => {
        /// Discovery, retry and lookup outcomes shared by all crawler tasks.
        #[derive(Default)]
        pub struct YieldRuntimeStats {
            $($name: AtomicU64,)+
            metadata_retry_waiting: AtomicU64,
            metadata_retry_history: AtomicU64,
            discovery_candidate_nodes: AtomicU64,
            discovery_cooldown_nodes: AtomicU64,
            discovery_samples_in_flight: AtomicU64,
        }

        impl YieldRuntimeStats {
            /// Increment one of the fixed, bounded counters.
            pub(crate) fn inc(&self, name: &'static str) {
                match name {
                    $(stringify!($name) => { self.$name.fetch_add(1, Ordering::Relaxed); }),+
                    _ => debug_assert!(false, "unknown yield counter: {name}"),
                }
                #[cfg(feature = "metrics")]
                metrics::counter!("dht_yield_events_total", "event" => name).increment(1);
            }

            /// Read counters without requiring the Prometheus exporter.
            pub fn snapshot(&self) -> std::collections::BTreeMap<String, u64> {
                [$( (stringify!($name).to_owned(), self.$name.load(Ordering::Relaxed)) ),+]
                    .into_iter().collect()
            }

            /// Set a fixed current-size gauge separately from monotonically increasing events.
            pub(crate) fn set(&self, name: &'static str, value: u64) {
                let gauge = match name {
                    "metadata_retry_waiting" => &self.metadata_retry_waiting,
                    "metadata_retry_history" => &self.metadata_retry_history,
                    "discovery_candidate_nodes" => &self.discovery_candidate_nodes,
                    "discovery_cooldown_nodes" => &self.discovery_cooldown_nodes,
                    "discovery_samples_in_flight" => &self.discovery_samples_in_flight,
                    _ => { debug_assert!(false, "unknown yield gauge: {name}"); return; }
                };
                gauge.store(value, Ordering::Relaxed);
                #[cfg(feature = "metrics")]
                metrics::gauge!("dht_yield_current", "state" => name).set(value as f64);
            }

            /// Load current state sizes without interpreting a decrease as a counter reset.
            pub fn gauges_snapshot(&self) -> std::collections::BTreeMap<String, u64> {
                [
                    ("metadata_retry_waiting", &self.metadata_retry_waiting),
                    ("metadata_retry_history", &self.metadata_retry_history),
                    ("discovery_candidate_nodes", &self.discovery_candidate_nodes),
                    ("discovery_cooldown_nodes", &self.discovery_cooldown_nodes),
                    ("discovery_samples_in_flight", &self.discovery_samples_in_flight),
                ].into_iter().map(|(name, gauge)|
                    (name.to_owned(), gauge.load(Ordering::Relaxed))).collect()
            }
        }
    };
}

yield_counters!(
    discovery_get_peers_observed,
    discovery_get_peers_rate_limited,
    discovery_get_peers_enqueued,
    discovery_sample_queries,
    discovery_sample_responses,
    discovery_sample_timeouts,
    discovery_sample_send_failed,
    discovery_sample_unsupported,
    discovery_sample_invalid,
    discovery_sample_unmatched,
    discovery_sample_no_new,
    discovery_sample_hashes_enqueued,
    discovery_hash_duplicate,
    discovery_hash_rate_limited,
    discovery_hash_queue_full,
    discovery_input_dropped,
    discovery_node_capacity_dropped,
    discovery_sample_cooldown_full,
    metadata_source_announce_started,
    metadata_source_announce_succeeded,
    metadata_source_announce_failed,
    metadata_source_get_peers_started,
    metadata_source_get_peers_succeeded,
    metadata_source_get_peers_failed,
    metadata_source_sample_started,
    metadata_source_sample_succeeded,
    metadata_source_sample_failed,
    metadata_retry_scheduled,
    metadata_retry_dispatched,
    metadata_retry_exhausted,
    metadata_retry_evicted,
    metadata_retry_deferred,
    metadata_retry_new_peer,
    metadata_retry_history_full,
    metadata_hash_only_started,
    lookup_finished_deadline,
    lookup_finished_budget,
    lookup_finished_exhausted,
    lookup_finished_peers,
    lookup_finished_canceled,
    lookup_source_seed_queries,
    lookup_request_queue_full,
);

#[cfg(test)]
mod yield_counter_tests {
    use super::super::DhtRuntimeStats;

    #[test]
    fn yield_counters_share_across_tasks_without_changing_runtime_snapshot() {
        let stats = DhtRuntimeStats::default();
        let clone = stats.clone();
        let worker = std::thread::spawn(move || {
            for _ in 0..1_000 {
                clone.yield_stats().inc("discovery_sample_queries");
            }
        });
        worker.join().unwrap();
        assert_eq!(
            stats.yield_stats().snapshot()["discovery_sample_queries"],
            1_000
        );
        assert_eq!(
            stats.yield_stats().snapshot()["metadata_retry_dispatched"],
            0
        );
        assert_eq!(stats.snapshot().hashes_received, 0);
    }
}
