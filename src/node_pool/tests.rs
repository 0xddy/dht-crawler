use super::recent::RecentProbes;
use super::*;
use ahash::AHashMap;
use rand::{Rng, SeedableRng};
use std::net::{IpAddr, Ipv4Addr};

    fn node(id: u8, addr: &str) -> NodeTuple {
        NodeTuple {
            id: [id; 20],
            addr: addr.parse().unwrap(),
        }
    }

    #[test]
    fn strict_fifo_replaces_oldest_after_warmup() {
        let start = Instant::now();
        let mut pool = NodePool::new(2, 600, Duration::from_secs(60), 1000, start);
        let first = node(1, "8.8.8.8:1");
        let second = node(2, "1.1.1.1:2");
        let third = node(3, "9.9.9.9:3");
        assert_eq!(pool.admit(first, start), AdmissionOutcome::Admitted);
        assert_eq!(pool.admit(second, start), AdmissionOutcome::Admitted);
        assert!(pool.is_warmed());
        assert_eq!(pool.admit(third, start), AdmissionOutcome::Replaced);
        assert_eq!(pool.front(), Some(second));
    }

    #[test]
    fn duplicate_does_not_reorder_fifo() {
        let start = Instant::now();
        let mut pool = NodePool::new(3, 600, Duration::from_secs(60), 1000, start);
        let first = node(1, "8.8.8.8:1");
        let second = node(2, "1.1.1.1:2");
        pool.admit(first, start);
        pool.admit(second, start);
        assert_eq!(pool.admit(first, start), AdmissionOutcome::Duplicate);
        assert_eq!(pool.front(), Some(first));
    }

    #[test]
    fn rotating_front_preserves_queued_dedup_and_recent_state() {
        let start = Instant::now();
        let mut pool = NodePool::new(3, 600, Duration::from_secs(60), 1000, start);
        let first = node(1, "8.8.8.8:1");
        let second = node(2, "1.1.1.1:2");

        assert_eq!(pool.admit(first, start), AdmissionOutcome::Admitted);
        assert_eq!(pool.admit(second, start), AdmissionOutcome::Admitted);
        assert!(pool.rotate_front_to_back());

        assert_eq!(pool.front(), Some(second));
        assert_eq!(pool.admit(first, start), AdmissionOutcome::Duplicate);
        assert!(!pool.contains_recent(&first.addr, start));
    }

    #[test]
    fn warmed_pool_enforces_replacement_rate() {
        let start = Instant::now();
        let mut pool = NodePool::new(2, 60, Duration::from_secs(60), 1000, start);
        pool.admit(node(1, "8.8.8.8:1"), start);
        pool.admit(node(2, "1.1.1.1:2"), start);
        assert_eq!(
            pool.admit(node(3, "9.9.9.9:3"), start),
            AdmissionOutcome::Replaced
        );
        assert_eq!(
            pool.admit(node(4, "208.67.222.222:4"), start),
            AdmissionOutcome::RateLimited
        );
        assert_eq!(
            pool.admit(node(4, "208.67.222.222:4"), start + Duration::from_secs(1)),
            AdmissionOutcome::Replaced
        );
    }

    #[test]
    fn recent_probe_blocks_readmission_until_expiry() {
        let start = Instant::now();
        let mut pool = NodePool::new(3, 600, Duration::from_secs(10), 1000, start);
        let first = node(1, "8.8.8.8:1");
        pool.admit(first, start);
        assert_eq!(pool.take_front_for_probe(start), Some(first));
        assert_eq!(pool.admit(first, start), AdmissionOutcome::Duplicate);
        assert_eq!(
            pool.admit(first, start + Duration::from_secs(11)),
            AdmissionOutcome::Admitted
        );
    }

    #[test]
    fn repeated_recent_probes_keep_exactly_one_expiry_and_restore_removes_it() {
        let start = Instant::now();
        let mut pool = NodePool::new(3, 600, Duration::from_secs(10), 100, start);
        let first = node(1, "8.8.8.8:1");
        for offset in 0..100_000 {
            pool.record_probe(first.addr, start + Duration::from_micros(offset));
        }
        assert_eq!(pool.recent_state(), (1, 100, 1));
        assert!(pool.contains_recent(&first.addr, start + Duration::from_secs(10)));
        assert!(!pool.contains_recent(&first.addr, start + Duration::from_secs(11)));
        assert_eq!(pool.recent_state(), (0, 100, 0));
        pool.admit(first, start + Duration::from_secs(12));
        let taken = pool
            .take_front_for_probe(start + Duration::from_secs(12))
            .unwrap();
        pool.restore_front(taken, start + Duration::from_secs(12));
        assert_eq!(pool.recent_state(), (0, 100, 0));
    }

    #[test]
    fn lazy_recent_refresh_does_not_hide_other_expired_addresses() {
        let start = Instant::now();
        let first = node(1, "8.8.8.8:1").addr;
        let second = node(2, "1.1.1.1:2").addr;
        let mut recent = RecentProbes::new(2);
        recent.record(first, start + Duration::from_secs(10));
        recent.record(second, start + Duration::from_secs(11));
        recent.record(first, start + Duration::from_secs(20));
        assert_eq!(recent.expire(start + Duration::from_secs(12)), 1);
        assert!(recent.contains(&first, start + Duration::from_secs(12)));
        assert!(!recent.contains(&second, start + Duration::from_secs(12)));
        assert_eq!(recent.expiry.len(), 1);
    }

    #[test]
    fn capacity_pressure_drains_due_checks_before_evicting_refreshed_addresses() {
        let start = Instant::now();
        let mut pool = NodePool::new(300, 60, Duration::from_secs(10), 300, start);
        for port in 1..=299 {
            pool.record_probe(SocketAddr::from(([8, 8, 8, 8], port)), start);
        }
        let expired = SocketAddr::from(([1, 1, 1, 1], 1));
        pool.record_probe(expired, start + Duration::from_secs(1));
        for port in 1..=299 {
            pool.record_probe(
                SocketAddr::from(([8, 8, 8, 8], port)),
                start + Duration::from_secs(5),
            );
        }
        let now = start + Duration::from_secs(11);
        let fresh = SocketAddr::from(([9, 9, 9, 9], 1));
        pool.record_probe(fresh, now);
        for port in 1..=299 {
            assert!(pool.contains_recent(&SocketAddr::from(([8, 8, 8, 8], port)), now));
        }
        assert!(!pool.contains_recent(&expired, now));
        assert!(pool.contains_recent(&fresh, now));
        assert_eq!(pool.recent_state(), (300, 300, 300));
    }

    #[test]
    fn recent_index_matches_reference_and_never_exceeds_its_capacity() {
        let start = Instant::now();
        let mut recent = RecentProbes::new(64);
        let mut reference = AHashMap::new();
        let mut rng = rand::rngs::StdRng::seed_from_u64(73);
        for tick in 0..10_000 {
            let now = start + Duration::from_millis(tick);
            let addr = SocketAddr::from(([8, 8, 8, 8], rng.gen_range(1..=64)));
            if rng.gen_ratio(1, 4) {
                recent.remove(&addr);
                reference.remove(&addr);
            } else {
                let expires = now + Duration::from_millis(30);
                recent.record(addr, expires);
                reference.insert(addr, expires);
            }
            recent.expire(now);
            reference.retain(|_, expires| *expires > now);
            assert_eq!(recent.entries.len(), reference.len());
            assert_eq!(recent.expiry.len(), reference.len());
            for (index, &(check_at, addr)) in recent.expiry.iter().enumerate() {
                assert_eq!(recent.entries[&addr].heap_index, index);
                assert_eq!(recent.entries[&addr].expires_at, reference[&addr]);
                assert!(check_at <= reference[&addr]);
                if index > 0 {
                    assert!(recent.expiry[(index - 1) / 2] <= recent.expiry[index]);
                }
            }
        }
        for port in 1..=1000 {
            recent.record(
                SocketAddr::from(([9, 9, 9, 9], port)),
                start + Duration::from_secs(100),
            );
            assert!(recent.entries.len() <= 64);
            assert_eq!(recent.entries.len(), recent.expiry.len());
        }
    }

    #[test]
    fn responsive_ring_overwrites_without_growing() {
        let start = Instant::now();
        let mut reservoir = ResponsiveReservoir::new(2, Duration::from_secs(10), Duration::ZERO);
        reservoir.record(node(1, "8.8.8.8:1"), start, start);
        reservoir.record(node(2, "1.1.1.1:2"), start, start);
        reservoir.record(node(3, "9.9.9.9:3"), start, start);
        let snapshot = reservoir.snapshot(10, start);
        assert_eq!(snapshot.len(), 2);
        assert!(!snapshot.iter().any(|entry| entry.id == [1; 20]));
        assert_eq!(reservoir.ready_len + reservoir.revisits.len(), 2);
    }

    #[test]
    fn snapshot_covers_id_prefixes_beyond_the_first_reservoir_slots() {
        let start = Instant::now();
        let mut reservoir = ResponsiveReservoir::new(32, Duration::from_secs(90), Duration::ZERO);
        for port in 1..=20 {
            reservoir.record(node(0, &format!("8.8.8.8:{port}")), start, start);
        }
        for prefix in [64, 128, 192] {
            reservoir.record(node(prefix, &format!("9.9.{prefix}.1:1")), start, start);
        }
        let snapshot = reservoir.snapshot(4, start);
        assert_eq!(snapshot.len(), 4);
        let prefixes: AHashSet<_> = snapshot.iter().map(|node| node.id[0]).collect();
        assert_eq!(prefixes, AHashSet::from_iter([0, 64, 128, 192]));
    }

    #[test]
    fn snapshot_prefers_distinct_subnets_then_fills_from_remaining_peers() {
        let start = Instant::now();
        let mut reservoir = ResponsiveReservoir::new(16, Duration::from_secs(90), Duration::ZERO);
        for port in 1..=8 {
            reservoir.record(node(0, &format!("8.8.8.8:{port}")), start, start);
        }
        reservoir.record(node(0, "1.1.1.1:1"), start, start);
        reservoir.record(node(0, "9.9.9.9:1"), start, start);
        let snapshot = reservoir.snapshot(3, start);
        assert_eq!(snapshot.len(), 3);
        let subnets: AHashSet<_> = snapshot
            .iter()
            .map(|node| SubnetKey::from_addr(&node.addr))
            .collect();
        assert_eq!(subnets.len(), 3);
        let all = reservoir.snapshot(16, start);
        assert_eq!(all.len(), 10);
        assert_eq!(
            all.iter()
                .map(|node| node.addr)
                .collect::<AHashSet<_>>()
                .len(),
            10
        );
    }

    #[test]
    fn small_snapshots_rotate_between_populated_prefixes_and_their_members() {
        let start = Instant::now();
        let mut reservoir = ResponsiveReservoir::new(16, Duration::from_secs(90), Duration::ZERO);
        for prefix in [0, 64, 192] {
            for port in 1..=4 {
                reservoir.record(
                    node(prefix, &format!("8.8.8.8:{}", u16::from(prefix) * 4 + port)),
                    start,
                    start,
                );
            }
        }
        let mut seen = AHashSet::new();
        for _ in 0..4 {
            let mut prefixes = AHashSet::new();
            for _ in 0..3 {
                let snapshot = reservoir.snapshot(1, start);
                assert_eq!(snapshot.len(), 1);
                assert!(prefixes.insert(snapshot[0].id[0]));
                assert!(seen.insert(snapshot[0].addr));
            }
        }
        assert_eq!(seen.len(), 12);
    }

    #[test]
    fn snapshot_and_coverage_exclude_expired_and_failed_nodes() {
        let start = Instant::now();
        let mut reservoir = ResponsiveReservoir::new(300, Duration::from_secs(10), Duration::ZERO);
        for prefix in 0..=u8::MAX {
            reservoir.record(
                node(prefix, &format!("8.8.8.8:{}", u16::from(prefix) + 1)),
                start,
                start,
            );
        }
        let now = start + Duration::from_secs(1);
        let failed = node(83, "8.8.8.8:84");
        assert!(reservoir.record_failure(failed.addr, now, now));
        let snapshot = reservoir.snapshot(16, now);
        assert!(!snapshot.iter().any(|node| node.addr == failed.addr));
        assert_eq!(reservoir.sparse_prefixes(), &[83]);
        // Coverage comes from all 255 eligible nodes, not the 16 published ones.
        reservoir.snapshot(1, now + Duration::from_secs(5));
        assert_eq!(reservoir.sparse_prefixes().len(), 256);
        assert!(
            reservoir
                .snapshot(300, start + Duration::from_secs(10))
                .is_empty()
        );
        assert_eq!(reservoir.sparse_prefixes().len(), 256);
    }

    #[test]
    fn snapshot_remains_complete_and_unique_under_skewed_prefixes_and_overwrites() {
        let start = Instant::now();
        let mut rng = rand::rngs::StdRng::seed_from_u64(417);
        let mut reservoir = ResponsiveReservoir::new(96, Duration::from_secs(10), Duration::ZERO);
        for port in 1..=192 {
            let prefix = if rng.gen_bool(0.8) {
                0
            } else {
                rng.gen_range(1..=255)
            };
            reservoir.record(
                node(prefix, &format!("8.8.{}.8:{port}", port % 9)),
                start,
                start,
            );
        }
        let now = start + Duration::from_secs(1);
        for port in 100..=110 {
            reservoir.record_failure(
                format!("8.8.{}.8:{port}", port % 9).parse().unwrap(),
                now,
                now,
            );
        }
        for limit in [0, 1, 2, 8, 32, 80, 200] {
            for _ in 0..8 {
                let snapshot = reservoir.snapshot(limit, now);
                let eligible: AHashSet<_> = reservoir
                    .slots
                    .iter()
                    .flatten()
                    .filter(|entry| entry.blocked_until.is_none_or(|deadline| deadline <= now))
                    .map(|entry| entry.node.addr)
                    .collect();
                let observed: AHashSet<_> = snapshot.iter().map(|node| node.addr).collect();
                assert_eq!(snapshot.len(), limit.min(eligible.len()));
                assert_eq!(observed.len(), snapshot.len());
                assert!(observed.is_subset(&eligible));
            }
        }
    }

    #[test]
    fn snapshot_scratch_stays_bounded_as_prefixes_change_and_slots_expire() {
        let start = Instant::now();
        let capacity = 96;
        let mut reservoir =
            ResponsiveReservoir::new(capacity, Duration::from_secs(1), Duration::ZERO);
        let mut scratch_capacities = None;
        for round in 0..256 {
            let now = start + Duration::from_secs(round * 2);
            let mut eligible = AHashSet::new();
            for index in 0..capacity {
                let prefix = if round % 2 == 0 {
                    round as u8
                } else {
                    index as u8
                };
                let port = round as usize * capacity + index + 1;
                let candidate = node(prefix, &format!("8.8.{}.8:{port}", index % 9));
                reservoir.record(candidate, now, now);
                if index % 11 == 0 {
                    reservoir.record_failure(candidate.addr, now, now);
                } else {
                    eligible.insert(candidate.addr);
                }
            }
            // Alternate short and full samples: a short refresh leaves unused
            // group/deferred links which must not enter the next snapshot.
            for limit in [capacity, 1, 0, 32, capacity * 2] {
                let snapshot = reservoir.snapshot(limit, now);
                let observed: AHashSet<_> = snapshot.iter().map(|node| node.addr).collect();
                assert_eq!(snapshot.len(), limit.min(eligible.len()));
                assert_eq!(observed.len(), snapshot.len());
                assert!(observed.is_subset(&eligible));
            }
            let scratch = &reservoir.snapshot_scratch;
            let capacities = (
                scratch.links.capacity(),
                scratch.subnets.capacity(),
                scratch.active.capacity(),
            );
            assert_eq!(scratch.links.len(), capacity);
            assert_eq!(*scratch_capacities.get_or_insert(capacities), capacities);
            assert!(
                reservoir
                    .snapshot(capacity, now + Duration::from_secs(1))
                    .is_empty()
            );
            assert_eq!(reservoir.sparse_prefixes().len(), 256);
        }
    }

    #[test]
    fn revisit_cooldown_starts_at_dispatch_and_replies_do_not_extend_it() {
        let start = Instant::now();
        let mut reservoir =
            ResponsiveReservoir::new(2, Duration::from_secs(90), Duration::from_secs(30));
        let first = node(1, "8.8.8.8:1");
        reservoir.record(first, start + Duration::from_secs(2), start);
        assert_eq!(
            reservoir.next_revisit(start + Duration::from_secs(29)),
            None
        );
        let dispatched = start + Duration::from_secs(30);
        assert_eq!(reservoir.next_revisit(dispatched), Some(first));
        reservoir.record_probe(first.addr, dispatched);
        reservoir.record(first, start + Duration::from_secs(32), dispatched);
        assert_eq!(
            reservoir.next_revisit(start + Duration::from_secs(59)),
            None
        );
        assert_eq!(
            reservoir.next_revisit(start + Duration::from_secs(60)),
            Some(first)
        );
    }

    #[test]
    fn warmed_pool_below_capacity_refills_without_replacement_rate_limit() {
        let start = Instant::now();
        let mut pool = NodePool::new(2, 60, Duration::from_secs(60), 1000, start);
        assert_eq!(pool.admit(node(1, "8.8.8.8:1"), start), AdmissionOutcome::Admitted);
        assert_eq!(pool.admit(node(2, "1.1.1.1:2"), start), AdmissionOutcome::Admitted);
        assert!(pool.is_warmed());
        // Exhaust replacement budget at full capacity.
        assert_eq!(pool.admit(node(3, "9.9.9.9:3"), start), AdmissionOutcome::Replaced);
        assert_eq!(
            pool.admit(node(4, "208.67.222.222:4"), start),
            AdmissionOutcome::RateLimited
        );
        // Pop both queued nodes so queue is below capacity; refilling must not be rate-limited.
        assert!(pool.take_front_for_probe(start).is_some());
        assert!(pool.take_front_for_probe(start).is_some());
        assert_eq!(pool.len(), 0);
        assert_eq!(
            pool.admit(node(4, "208.67.222.222:4"), start),
            AdmissionOutcome::Admitted
        );
        assert_eq!(
            pool.admit(node(5, "4.4.4.4:5"), start),
            AdmissionOutcome::Admitted
        );
    }

    #[test]
    fn zero_revisit_interval_keeps_immediate_round_robin() {
        let start = Instant::now();
        let mut reservoir = ResponsiveReservoir::new(2, Duration::from_secs(10), Duration::ZERO);
        let first = node(1, "8.8.8.8:1");
        let second = node(2, "1.1.1.1:2");
        reservoir.record(first, start, start);
        reservoir.record(second, start, start);
        for expected in (0..10_000).map(|index| if index % 2 == 0 { first } else { second }) {
            assert_eq!(reservoir.next_revisit(start), Some(expected));
            reservoir.record_probe(expected.addr, start);
            reservoir.record(expected, start, start);
            assert_eq!(reservoir.ready_len + reservoir.revisits.len(), 2);
            assert!(
                reservoir.revisits.is_empty(),
                "zero interval must avoid the time index"
            );
        }
    }

    #[test]
    fn in_flight_responsive_node_is_not_re_dispatched_until_reply_or_failure() {
        let start = Instant::now();
        let first = node(1, "8.8.8.8:1");
        let mut reservoir = ResponsiveReservoir::new(2, Duration::from_secs(60), Duration::ZERO);
        reservoir.record(first, start, start);
        assert_eq!(reservoir.next_revisit(start), Some(first));
        reservoir.record_probe(first.addr, start);
        // While in flight, the endpoint cannot be selected again even with revisit_interval == 0.
        assert_eq!(reservoir.next_revisit(start + Duration::from_secs(10)), None);
        reservoir.defer_revisit(first.addr, start, start);
        assert_eq!(reservoir.next_revisit(start + Duration::from_secs(10)), None);
        // A reply makes it eligible again.
        reservoir.record(first, start + Duration::from_secs(1), start);
        assert_eq!(reservoir.next_revisit(start + Duration::from_secs(1)), Some(first));
        reservoir.record_probe(first.addr, start + Duration::from_secs(2));
        assert_eq!(reservoir.next_revisit(start + Duration::from_secs(2)), None);
        // Failure clears in_flight and schedules backoff.
        assert!(reservoir.record_failure(
            first.addr,
            start + Duration::from_secs(2),
            start + Duration::from_secs(3)
        ));
        assert_eq!(reservoir.next_revisit(start + Duration::from_secs(7)), None);
        assert_eq!(reservoir.next_revisit(start + Duration::from_secs(8)), Some(first));
    }

    #[test]
    fn responsive_failures_back_off_with_a_cap_and_success_resets_them() {
        let start = Instant::now();
        let first = node(1, "8.8.8.8:1");
        let mut reservoir = ResponsiveReservoir::new(2, Duration::from_secs(900), Duration::ZERO);
        reservoir.record(first, start, start);
        let mut now = start + Duration::from_secs(1);
        for seconds in [5, 10, 20, 40, 60, 60] {
            assert_eq!(reservoir.next_revisit(now), Some(first));
            reservoir.record_probe(first.addr, now);
            assert!(reservoir.record_failure(first.addr, now, now));
            assert_eq!(
                reservoir.next_revisit(now + Duration::from_secs(seconds - 1)),
                None
            );
            now += Duration::from_secs(seconds);
        }
        reservoir.record(first, now, now - Duration::from_secs(1));
        assert_eq!(reservoir.next_revisit(now), Some(first));
        reservoir.record_probe(first.addr, now);
        assert!(reservoir.record_failure(first.addr, now, now));
        assert_eq!(reservoir.next_revisit(now + Duration::from_secs(4)), None);
        assert_eq!(
            reservoir.next_revisit(now + Duration::from_secs(5)),
            Some(first)
        );
    }

    #[test]
    fn old_failure_cannot_override_a_newer_success_or_leave_duplicate_schedule_entries() {
        let start = Instant::now();
        let first = node(1, "8.8.8.8:1");
        let second = node(2, "1.1.1.1:2");
        let mut reservoir = ResponsiveReservoir::new(1, Duration::from_secs(90), Duration::ZERO);
        reservoir.record(first, start + Duration::from_secs(2), start);
        assert!(!reservoir.record_failure(first.addr, start, start + Duration::from_secs(3)));
        assert!(reservoir.record_failure(
            first.addr,
            start + Duration::from_secs(3),
            start + Duration::from_secs(4)
        ));
        reservoir.record(
            second,
            start + Duration::from_secs(5),
            start + Duration::from_secs(5),
        );
        assert!(
            reservoir.revisits.is_empty(),
            "overwriting must remove the old delayed entry"
        );
        assert_eq!(
            reservoir.next_revisit(start + Duration::from_secs(5)),
            Some(second)
        );
        reservoir.record_probe(second.addr, start + Duration::from_secs(5));
        reservoir.record(
            second,
            start + Duration::from_secs(5),
            start + Duration::from_secs(5),
        );
        assert_eq!(reservoir.ready_len, 1);
    }

    #[test]
    fn ready_list_stays_bounded_and_fair_after_overwrites_and_delayed_failures() {
        let start = Instant::now();
        let mut reservoir = ResponsiveReservoir::new(3, Duration::from_secs(90), Duration::ZERO);
        let nodes = [
            node(1, "8.8.8.8:1"),
            node(2, "1.1.1.1:2"),
            node(3, "9.9.9.9:3"),
            node(4, "4.4.4.4:4"),
        ];
        for current in &nodes[..3] {
            reservoir.record(*current, start, start);
        }
        assert_eq!(reservoir.next_revisit(start), Some(nodes[0]));
        reservoir.record_probe(nodes[0].addr, start);
        reservoir.record(nodes[0], start, start);
        reservoir.record(nodes[3], start, start); // Overwrite the old tail.
        let now = start + Duration::from_secs(1);
        assert!(reservoir.record_failure(nodes[2].addr, now, now)); // Remove the middle.
        for expected in [nodes[1], nodes[3], nodes[1], nodes[3]] {
            assert_eq!(reservoir.next_revisit(now), Some(expected));
            reservoir.record_probe(expected.addr, now);
            reservoir.record(expected, now, now);
        }
        let ready_at = now + Duration::from_secs(5);
        let mut found = AHashSet::new();
        for _ in 0..3 {
            let current = reservoir.next_revisit(ready_at).unwrap();
            assert!(found.insert(current.addr));
            reservoir.record_probe(current.addr, ready_at);
            reservoir.record(current, ready_at, ready_at);
        }
        assert_eq!(reservoir.ready_len, 3);
        assert!(reservoir.revisits.is_empty());
    }

    #[test]
    fn revisit_expiry_cleanup_is_bounded_and_removes_time_index_entries() {
        let start = Instant::now();
        let mut reservoir = ResponsiveReservoir::new(300, Duration::from_secs(10), Duration::ZERO);
        for port in 1..=300 {
            reservoir.record(node(1, &format!("8.8.8.8:{port}")), start, start);
        }
        assert_eq!(
            reservoir.next_revisit(start + Duration::from_secs(11)),
            None
        );
        assert_eq!(reservoir.ready_len + reservoir.revisits.len(), 44);
        assert_eq!(
            reservoir.next_revisit(start + Duration::from_secs(11)),
            None
        );
        assert!(reservoir.revisits.is_empty());
        assert!(reservoir.index.is_empty());
    }

    #[test]
    #[ignore = "release-only FIFO throughput smoke test"]
    fn million_fifo_operations() {
        let start = Instant::now();
        let mut pool = NodePool::new(100_000, u32::MAX, Duration::from_secs(600), 100_000, start);
        for value in 0..1_000_000u32 {
            let octets = value.to_be_bytes();
            let node = NodeTuple {
                id: [octets[3]; 20],
                addr: SocketAddr::new(
                    IpAddr::V4(Ipv4Addr::new(11, octets[1], octets[2], octets[3])),
                    (value % 65_534 + 1) as u16,
                ),
            };
            let outcome = pool.admit(node, start);
            assert!(!matches!(outcome, AdmissionOutcome::RateLimited));
        }
        assert_eq!(pool.len(), 100_000);
        eprintln!("1,000,000 FIFO admissions in {:?}", start.elapsed());
    }
