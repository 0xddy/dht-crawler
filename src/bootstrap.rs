use crate::addr::{addr_allowed_by_netmode, is_valid_node_addr};
use crate::crawl_config::ResolvedCrawlConfig;
use crate::types::NetMode;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

pub(crate) struct BootstrapGate {
    last_bootstrap: Option<Instant>,
}

impl BootstrapGate {
    pub(crate) fn new() -> Self {
        Self {
            last_bootstrap: None,
        }
    }

    pub(crate) fn should_bootstrap(
        &mut self,
        pool_len: usize,
        config: &ResolvedCrawlConfig,
        now: Instant,
    ) -> bool {
        if config.bootstrap_max_nodes_per_round == 0 {
            return false;
        }
        if pool_len >= config.low_watermark {
            return false;
        }
        if let Some(last) = self.last_bootstrap
            && now.checked_duration_since(last).unwrap_or_default() < config.bootstrap_interval
        {
            return false;
        }
        self.last_bootstrap = Some(now);
        true
    }
}

#[derive(Default)]
struct BootstrapSourceState {
    last_attempt: Option<Instant>,
    last_success: Option<Instant>,
    fail_count: u32,
    backoff_until: Option<Instant>,
}

pub(crate) struct BootstrapSourcePool {
    pub(crate) hosts: Vec<String>,
    states: HashMap<SocketAddr, BootstrapSourceState>,
    backoff_base: Duration,
    backoff_max: Duration,
}

impl BootstrapSourcePool {
    pub(crate) fn new(hosts: Vec<String>, backoff_base: Duration, backoff_max: Duration) -> Self {
        Self {
            hosts,
            states: HashMap::new(),
            backoff_base,
            backoff_max,
        }
    }

    pub(crate) fn select(
        &mut self,
        candidates: Vec<SocketAddr>,
        max_nodes: usize,
        now: Instant,
    ) -> Vec<SocketAddr> {
        let mut selected = Vec::with_capacity(max_nodes);
        let mut seen = HashSet::with_capacity(candidates.len());
        let mut earliest_backoff: Option<(SocketAddr, Instant)> = None;

        for addr in candidates {
            if !seen.insert(addr) {
                continue;
            }

            let state = self.states.entry(addr).or_default();
            if let Some(backoff_until) = state.backoff_until
                && backoff_until > now
            {
                if earliest_backoff.is_none_or(|(_, current)| backoff_until < current) {
                    earliest_backoff = Some((addr, backoff_until));
                }
                continue;
            }

            selected.push(addr);
            if selected.len() >= max_nodes {
                return selected;
            }
        }

        if selected.is_empty()
            && max_nodes > 0
            && let Some((addr, _)) = earliest_backoff
        {
            selected.push(addr);
        }
        selected
    }

    pub(crate) fn mark_attempt(&mut self, addr: SocketAddr, now: Instant) {
        self.states.entry(addr).or_default().last_attempt = Some(now);
    }

    pub(crate) fn mark_success(&mut self, addr: SocketAddr, now: Instant) {
        let state = self.states.entry(addr).or_default();
        state.last_success = Some(now);
        state.fail_count = 0;
        state.backoff_until = None;
    }

    pub(crate) fn mark_timeout(&mut self, addr: SocketAddr, now: Instant) {
        let state = self.states.entry(addr).or_default();
        state.fail_count = state.fail_count.saturating_add(1);
        let multiplier = 1u32
            .checked_shl(state.fail_count.saturating_sub(1).min(16))
            .unwrap_or(u32::MAX);
        let backoff = self
            .backoff_base
            .saturating_mul(multiplier)
            .min(self.backoff_max);
        state.backoff_until = Some(now + backoff);
    }
}

pub(crate) async fn resolve_bootstrap_nodes(hosts: &[String], netmode: NetMode) -> Vec<SocketAddr> {
    let mut resolved = Vec::new();
    for host in hosts {
        if let Ok(addrs) = tokio::net::lookup_host(host).await {
            for addr in addrs {
                if !addr_allowed_by_netmode(&addr, netmode) || !is_valid_node_addr(&addr) {
                    continue;
                }
                resolved.push(addr);
            }
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CrawlOptions;

    fn test_config() -> ResolvedCrawlConfig {
        ResolvedCrawlConfig::from_options(&CrawlOptions::default())
    }

    #[test]
    fn bootstrap_pool_backs_off_dead_sources_without_spending_quota() {
        let start = Instant::now();
        let addr1: SocketAddr = "8.8.8.8:6881".parse().unwrap();
        let addr2: SocketAddr = "1.1.1.1:6881".parse().unwrap();
        let mut pool = BootstrapSourcePool::new(
            vec!["example.invalid:6881".to_string()],
            Duration::from_secs(300),
            Duration::from_secs(3600),
        );

        pool.mark_timeout(addr1, start);
        let selected = pool.select(vec![addr1, addr2], 1, start + Duration::from_secs(1));
        assert_eq!(selected, vec![addr2]);

        pool.mark_success(addr1, start + Duration::from_secs(2));
        let selected = pool.select(vec![addr1], 1, start + Duration::from_secs(3));
        assert_eq!(selected, vec![addr1]);
    }

    #[test]
    fn bootstrap_pool_forces_one_retry_when_all_sources_backed_off() {
        let start = Instant::now();
        let addr1: SocketAddr = "8.8.8.8:6881".parse().unwrap();
        let addr2: SocketAddr = "1.1.1.1:6881".parse().unwrap();
        let mut pool = BootstrapSourcePool::new(
            vec!["example.invalid:6881".to_string()],
            Duration::from_secs(300),
            Duration::from_secs(3600),
        );
        pool.mark_timeout(addr1, start);
        pool.mark_timeout(addr2, start);

        let selected = pool.select(vec![addr1, addr2], 3, start + Duration::from_secs(1));
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn bootstrap_pool_deduplicates_resolved_addresses() {
        let start = Instant::now();
        let addr: SocketAddr = "8.8.8.8:6881".parse().unwrap();
        let mut pool = BootstrapSourcePool::new(
            vec!["example.invalid:6881".to_string()],
            Duration::from_secs(300),
            Duration::from_secs(3600),
        );

        let selected = pool.select(vec![addr, addr], 10, start);
        assert_eq!(selected, vec![addr]);
    }

    #[test]
    fn bootstrap_gate_uses_pool_low_water_mark() {
        let start = Instant::now();
        let config = test_config();
        let mut gate = BootstrapGate::new();

        assert!(!gate.should_bootstrap(config.low_watermark, &config, start));
        assert!(gate.should_bootstrap(0, &config, start));
        assert!(!gate.should_bootstrap(
            999,
            &config,
            start + config.bootstrap_interval - Duration::from_secs(1)
        ));
        assert!(gate.should_bootstrap(999, &config, start + config.bootstrap_interval));
    }
}
