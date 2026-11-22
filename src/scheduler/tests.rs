use super::queue::{HASH_QUEUE_TTL, InFlightState, PeerCandidate, PendingHashQueue, QueuePushKind};
use super::race::{JobPeerLookup, race_peer_fetches};
use super::retry::{MAX_DEFER_RECHECKS, MAX_RETRY_LIFETIME, RetryHistory, RetryRecord};
use super::*;
use crate::metadata::MetadataFetchOutcome;
use crate::runtime_stats::DhtRuntimeLimits;
use crate::types::NodeTuple;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;

    struct DropSignal(Arc<AtomicBool>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    fn empty_torrent_callback() -> Arc<ArcSwapOption<TorrentAckCallback>> {
        Arc::new(ArcSwapOption::empty())
    }

    fn fetch_gate<F, Fut>(callback: F) -> Arc<ArcSwapOption<MetadataFetchCallback>>
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = bool> + Send + 'static,
    {
        let holder = Arc::new(ArcSwapOption::empty());
        let callback: Arc<MetadataFetchCallback> = Arc::new(Box::new(move |hash| {
            let future = callback(hash);
            Box::pin(async move {
                if future.await {
                    MetadataAdmission::Fetch
                } else {
                    MetadataAdmission::Skip
                }
            })
        }));
        holder.store(Some(callback));
        holder
    }

    fn completion_callback<F>(callback: F) -> Arc<ArcSwapOption<MetadataCompletionCallback>>
    where
        F: Fn(MetadataFetchCompletion) + Send + Sync + 'static,
    {
        let holder = Arc::new(ArcSwapOption::empty());
        let callback: Arc<MetadataCompletionCallback> = Arc::new(Box::new(callback));
        holder.store(Some(callback));
        holder
    }

    fn event(hash: &str, port: u16, discovered_at: Instant) -> HashDiscovered {
        HashDiscovered {
            info_hash: hash.to_string(),
            peer_addr: Some(SocketAddr::from(([127, 0, 0, 1], port))),
            source_node: None,
            source: HashSource::Announce,
            discovered_at,
        }
    }

    fn lookup_request_channel(
        capacity: usize,
    ) -> (JobPeerLookup, mpsc::Receiver<PeerLookupRequest>) {
        let (request_tx, request_rx) = mpsc::channel(capacity);
        (
            JobPeerLookup {
                request_tx,
                info_hash: [3; 20],
                source_nodes: Vec::new(),
                source: HashSource::GetPeers,
                timeout: Duration::from_secs(2),
            },
            request_rx,
        )
    }

    #[test]
    fn runtime_stats_track_queue_outcomes_and_depth() {
        let (_hash_tx, hash_rx) = mpsc::channel(4);
        let stats = DhtRuntimeStats::with_limits(DhtRuntimeLimits {
            metadata_queue: 2,
            ..DhtRuntimeLimits::default()
        });
        let scheduler = MetadataScheduler::new_with_runtime_stats(
            hash_rx,
            Arc::new(RbitFetcher::new(1)),
            MetadataSchedulerLimits {
                queue_size: 2,
                concurrency: 1,
            },
            MetadataSchedulerCallbacks {
                torrent: empty_torrent_callback(),
                fetch_gate: Arc::new(ArcSwapOption::empty()),
                completion: Arc::new(ArcSwapOption::empty()),
            },
            Arc::new(AtomicUsize::new(0)),
            CancellationToken::new(),
            stats.clone(),
        );
        let start = Instant::now();
        let mut queue = PendingHashQueue::new(2, HASH_QUEUE_TTL);
        let mut in_flight = HashMap::new();

        scheduler.enqueue(&mut queue, &mut in_flight, event("old", 1000, start));
        scheduler.enqueue(
            &mut queue,
            &mut in_flight,
            event("old", 1001, start + Duration::from_secs(1)),
        );
        scheduler.enqueue(
            &mut queue,
            &mut in_flight,
            event("middle", 1002, start + Duration::from_secs(2)),
        );
        scheduler.enqueue(
            &mut queue,
            &mut in_flight,
            event("new", 1003, start + Duration::from_secs(3)),
        );
        scheduler.enqueue(
            &mut queue,
            &mut in_flight,
            event(
                "stale",
                1004,
                start
                    .checked_sub(HASH_QUEUE_TTL + Duration::from_secs(1))
                    .unwrap(),
            ),
        );
        scheduler.sync_queue_len(queue.len(), queue.pressure_len(), 1);

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.metadata_queue_inserted, 3);
        assert_eq!(snapshot.metadata_queue_deduplicated, 1);
        assert_eq!(snapshot.metadata_queue_evicted, 1);
        assert_eq!(snapshot.metadata_queue_stale, 1);
        assert_eq!(snapshot.metadata_queue_depth, 3);
        assert_eq!(snapshot.metadata_queue_max, 2);
        assert_eq!(snapshot.metadata_in_flight, 1);
    }

    #[test]
    fn queue_deduplicates_hash_and_keeps_bounded_newest_unique_peers() {
        let start = Instant::now();
        let mut queue = PendingHashQueue::new(10, Duration::from_secs(120));
        for offset in 0..=MAX_METADATA_CANDIDATES_PER_HASH as u16 {
            assert_ne!(
                queue.push(
                    event(
                        "hash",
                        1000 + offset,
                        start + Duration::from_secs(offset as u64)
                    ),
                    start + Duration::from_secs(offset as u64),
                ),
                QueuePushKind::Stale
            );
        }
        queue.push(
            event(
                "hash",
                1000 + MAX_METADATA_CANDIDATES_PER_HASH as u16,
                start + Duration::from_secs(100),
            ),
            start + Duration::from_secs(100),
        );

        assert_eq!(queue.len(), 1);
        let entry = queue.remove("hash").unwrap();
        let ports: Vec<_> = entry.peers.iter().map(|peer| peer.addr.port()).collect();
        assert_eq!(
            ports,
            (1001..=1000 + MAX_METADATA_CANDIDATES_PER_HASH as u16)
                .rev()
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn coalescing_window_is_fixed_from_first_enqueue() {
        let start = Instant::now();
        let mut queue = PendingHashQueue::new(10, Duration::from_secs(60));
        let in_flight = HashMap::new();
        queue.push(event("hash", 1000, start), start);
        queue.push(
            event("hash", 1001, start + Duration::from_millis(15)),
            start + Duration::from_millis(15),
        );

        assert!(
            queue
                .pop_newest_ready(start + Duration::from_millis(24), &in_flight)
                .is_none()
        );
        let entry = queue
            .pop_newest_ready(start + Duration::from_millis(25), &in_flight)
            .expect("duplicate arrivals must not extend the coalescing deadline");
        assert_eq!(entry.peers.len(), 2);
    }

    #[test]
    fn in_flight_hash_routes_bounded_unique_candidates_to_live_race() {
        let (_hash_tx, hash_rx) = mpsc::channel(4);
        let stats = DhtRuntimeStats::default();
        let scheduler = MetadataScheduler::new_with_runtime_stats(
            hash_rx,
            Arc::new(RbitFetcher::new(1)),
            MetadataSchedulerLimits {
                queue_size: 4,
                concurrency: 1,
            },
            MetadataSchedulerCallbacks {
                torrent: empty_torrent_callback(),
                fetch_gate: Arc::new(ArcSwapOption::empty()),
                completion: Arc::new(ArcSwapOption::empty()),
            },
            Arc::new(AtomicUsize::new(0)),
            CancellationToken::new(),
            stats.clone(),
        );
        let (peer_tx, mut peer_rx) = mpsc::channel(MAX_METADATA_CANDIDATES_PER_HASH);
        let mut in_flight = HashMap::from([(
            "hash".to_string(),
            InFlightState {
                peer_tx,
                scheduled_peers: HashSet::from([SocketAddr::from(([127, 0, 0, 1], 1000))]),
                entry: {
                    let mut queue = PendingHashQueue::new(1, HASH_QUEUE_TTL);
                    queue.push(event("hash", 1000, Instant::now()), Instant::now());
                    queue.remove("hash").unwrap()
                },
            },
        )]);
        let mut queue = PendingHashQueue::new(4, HASH_QUEUE_TTL);
        let now = Instant::now();

        for port in 1001..=1001 + MAX_METADATA_CANDIDATES_PER_HASH as u16 {
            scheduler.enqueue(&mut queue, &mut in_flight, event("hash", port, now));
        }
        scheduler.enqueue(&mut queue, &mut in_flight, event("hash", 1001, now));

        let mut received = Vec::new();
        while let Ok(peer) = peer_rx.try_recv() {
            received.push(peer.addr.port());
        }
        assert_eq!(
            received,
            (1001..1000 + MAX_METADATA_CANDIDATES_PER_HASH as u16).collect::<Vec<_>>()
        );
        assert_eq!(
            in_flight["hash"].scheduled_peers.len(),
            MAX_METADATA_CANDIDATES_PER_HASH
        );
        assert!(queue.is_empty());
        let snapshot = stats.snapshot();
        // Enqueue only forwards candidates; joining is counted after cross-source race deduplication.
        assert_eq!(snapshot.metadata_live_peer_joins, 0);
        assert_eq!(
            snapshot.metadata_queue_deduplicated,
            MAX_METADATA_CANDIDATES_PER_HASH as u64 + 2
        );
    }

    #[tokio::test]
    async fn peer_race_returns_first_success_and_cancels_remaining_fetches() {
        let now = Instant::now();
        let peers = vec![
            PeerCandidate {
                addr: SocketAddr::from(([127, 0, 0, 1], 1000)),
                discovered_at: now,
            },
            PeerCandidate {
                addr: SocketAddr::from(([127, 0, 0, 1], 1001)),
                discovered_at: now,
            },
            PeerCandidate {
                addr: SocketAddr::from(([127, 0, 0, 1], 1002)),
                discovered_at: now,
            },
        ];
        let started = Arc::new(AtomicUsize::new(0));
        let cancelled = Arc::new(AtomicBool::new(false));
        let barrier = Arc::new(tokio::sync::Barrier::new(peers.len()));
        let started_for_race = started.clone();
        let cancelled_for_race = cancelled.clone();
        let (_peer_tx, mut peer_rx) = mpsc::channel(MAX_METADATA_CANDIDATES_PER_HASH);
        let stats = DhtRuntimeStats::default();
        let (lookup, mut request_rx) = lookup_request_channel(1);

        let result = race_peer_fetches(
            peers,
            &mut peer_rx,
            Some(lookup),
            move |peer| {
                let started = started_for_race.clone();
                let cancelled = cancelled_for_race.clone();
                let barrier = barrier.clone();
                async move {
                    started.fetch_add(1, Ordering::Relaxed);
                    barrier.wait().await;
                    match peer.addr.port() {
                        1000 => {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            (
                                peer,
                                MetadataFetchOutcome::Fetched((
                                    "winner".to_string(),
                                    1,
                                    Vec::new(),
                                    0,
                                )),
                            )
                        }
                        1001 => {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            (peer, MetadataFetchOutcome::Failed)
                        }
                        _ => {
                            let _signal = DropSignal(cancelled);
                            std::future::pending::<()>().await;
                            unreachable!()
                        }
                    }
                }
            },
            &stats,
        )
        .await
        .expect("one peer should win the race");

        assert_eq!(result.0.addr.port(), 1000);
        assert_eq!(result.1.0, "winner");
        assert!(cancelled.load(Ordering::Relaxed));
        assert!(
            matches!(
                request_rx.try_recv(),
                Err(mpsc::error::TryRecvError::Disconnected)
            ),
            "fast success must drop the delayed lookup without leaving a task behind"
        );
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.metadata_races_started, 1);
        assert_eq!(snapshot.metadata_peer_candidates, 3);
        assert_eq!(snapshot.metadata_peer_canceled, 2);
    }

    #[tokio::test]
    async fn live_peer_joins_running_race_and_cancels_slow_initial_peer() {
        let now = Instant::now();
        let initial = vec![PeerCandidate {
            addr: SocketAddr::from(([127, 0, 0, 1], 1000)),
            discovered_at: now,
        }];
        let live = PeerCandidate {
            addr: SocketAddr::from(([127, 0, 0, 1], 1001)),
            discovered_at: now,
        };
        let (peer_tx, mut peer_rx) = mpsc::channel(MAX_METADATA_CANDIDATES_PER_HASH);
        let initial_started = Arc::new(tokio::sync::Notify::new());
        let initial_started_for_fetch = initial_started.clone();
        let initial_cancelled = Arc::new(AtomicBool::new(false));
        let initial_cancelled_for_fetch = initial_cancelled.clone();
        let stats = DhtRuntimeStats::default();
        let sender = tokio::spawn(async move {
            initial_started.notified().await;
            peer_tx.send(live).await.unwrap();
        });

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            race_peer_fetches(
                initial,
                &mut peer_rx,
                None,
                move |peer| {
                    let initial_started = initial_started_for_fetch.clone();
                    let initial_cancelled = initial_cancelled_for_fetch.clone();
                    async move {
                        if peer.addr.port() == 1000 {
                            let _signal = DropSignal(initial_cancelled);
                            initial_started.notify_one();
                            std::future::pending::<()>().await;
                            unreachable!()
                        }
                        (
                            peer,
                            MetadataFetchOutcome::Fetched((
                                "live-winner".to_string(),
                                1,
                                Vec::new(),
                                0,
                            )),
                        )
                    }
                },
                &stats,
            ),
        )
        .await
        .unwrap()
        .expect("live peer should win");
        sender.await.unwrap();

        assert_eq!(result.0.addr.port(), 1001);
        assert!(initial_cancelled.load(Ordering::Relaxed));
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.metadata_peer_candidates, 2);
        assert_eq!(snapshot.metadata_live_peer_joins, 1);
        assert_eq!(snapshot.metadata_peer_canceled, 1);
    }

    #[tokio::test]
    async fn lookup_and_announce_of_same_peer_count_only_one_live_join() {
        let (_hash_tx, hash_rx) = mpsc::channel(4);
        let stats = DhtRuntimeStats::default();
        let scheduler = MetadataScheduler::new_with_runtime_stats(
            hash_rx,
            Arc::new(RbitFetcher::new(1)),
            MetadataSchedulerLimits {
                queue_size: 4,
                concurrency: 1,
            },
            MetadataSchedulerCallbacks {
                torrent: empty_torrent_callback(),
                fetch_gate: Arc::new(ArcSwapOption::empty()),
                completion: Arc::new(ArcSwapOption::empty()),
            },
            Arc::new(AtomicUsize::new(0)),
            CancellationToken::new(),
            stats.clone(),
        );
        let initial = PeerCandidate {
            addr: SocketAddr::from(([127, 0, 0, 1], 1000)),
            discovered_at: Instant::now(),
        };
        let duplicate_addr = SocketAddr::from(([127, 0, 0, 1], 1001));
        let (peer_tx, mut peer_rx) = mpsc::channel(MAX_METADATA_CANDIDATES_PER_HASH);
        let (lookup, mut request_rx) = lookup_request_channel(1);
        let lookup_peer_started = Arc::new(tokio::sync::Notify::new());
        let started_for_actor = lookup_peer_started.clone();
        let actor = tokio::spawn(async move {
            let request = request_rx.recv().await.unwrap();
            request.peer_tx.send(duplicate_addr).await.unwrap();
            started_for_actor.notified().await;

            // The outer scheduler has not seen this lookup endpoint. Its announce is
            // forwarded, but must not count as another join in the active race.
            let mut in_flight = HashMap::from([(
                "hash".to_string(),
                InFlightState {
                    peer_tx,
                    scheduled_peers: HashSet::from([initial.addr]),
                    entry: {
                        let mut queue = PendingHashQueue::new(1, HASH_QUEUE_TTL);
                        queue.push(event("hash", 1000, Instant::now()), Instant::now());
                        queue.remove("hash").unwrap()
                    },
                },
            )]);
            let mut queue = PendingHashQueue::new(4, HASH_QUEUE_TTL);
            scheduler.enqueue(
                &mut queue,
                &mut in_flight,
                event("hash", 1001, Instant::now()),
            );
            assert!(in_flight["hash"].scheduled_peers.contains(&duplicate_addr));
        });
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            race_peer_fetches(
                vec![initial],
                &mut peer_rx,
                Some(lookup),
                move |peer| {
                    let started = lookup_peer_started.clone();
                    async move {
                        if peer.addr == duplicate_addr {
                            started.notify_one();
                        }
                        (peer, MetadataFetchOutcome::Failed)
                    }
                },
                &stats,
            ),
        )
        .await
        .unwrap();
        actor.await.unwrap();
        assert!(result.is_none());
        assert_eq!(stats.snapshot().metadata_peer_candidates, 2);
        assert_eq!(stats.snapshot().metadata_live_peer_joins, 1);
    }

    #[tokio::test]
    async fn lookup_peer_arriving_after_idle_grace_can_win_after_initial_peer_failed() {
        let initial = PeerCandidate {
            addr: SocketAddr::from(([127, 0, 0, 1], 1000)),
            discovered_at: Instant::now(),
        };
        let (_peer_tx, mut peer_rx) = mpsc::channel(12);
        let (lookup, mut request_rx) = lookup_request_channel(1);
        let actor = tokio::spawn(async move {
            let request = request_rx.recv().await.unwrap();
            tokio::time::sleep(Duration::from_millis(400)).await;
            request.peer_tx.send(initial.addr).await.unwrap();
            request
                .peer_tx
                .send(SocketAddr::from(([127, 0, 0, 1], 1001)))
                .await
                .unwrap();
            // Closing immediately must not make completion overtake the buffered winner.
        });
        let stats = DhtRuntimeStats::default();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            race_peer_fetches(
                vec![initial],
                &mut peer_rx,
                Some(lookup),
                |peer| async move {
                    let outcome = if peer.addr.port() == 1000 {
                        MetadataFetchOutcome::SkippedCached
                    } else {
                        MetadataFetchOutcome::Fetched(("late-winner".to_string(), 1, Vec::new(), 0))
                    };
                    (peer, outcome)
                },
                &stats,
            ),
        )
        .await
        .unwrap()
        .expect("pending lookup must survive the 300 ms idle grace");
        actor.await.unwrap();
        assert_eq!(result.0.addr.port(), 1001);
        assert_eq!(
            stats.snapshot().metadata_peer_candidates,
            2,
            "duplicate lookup peer must not consume a slot"
        );
    }

    #[tokio::test]
    async fn twelve_cached_candidates_allow_a_late_lookup_winner() {
        let (_peer_tx, mut peer_rx) = mpsc::channel(MAX_METADATA_CANDIDATES_PER_HASH);
        let (lookup, mut request_rx) = lookup_request_channel(1);
        let actor = tokio::spawn(async move {
            let request = request_rx.recv().await.unwrap();
            tokio::time::sleep(Duration::from_millis(400)).await;
            request
                .peer_tx
                .send(SocketAddr::from(([127, 0, 0, 1], 2000)))
                .await
                .unwrap();
        });
        let peers = (1000..1012)
            .map(|port| PeerCandidate {
                addr: SocketAddr::from(([127, 0, 0, 1], port)),
                discovered_at: Instant::now(),
            })
            .collect();
        let actual = Arc::new(AtomicUsize::new(0));
        let recorded = actual.clone();
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            race_peer_fetches(
                peers,
                &mut peer_rx,
                Some(lookup),
                move |peer| {
                    let recorded = recorded.clone();
                    async move {
                        let outcome = if peer.addr.port() == 2000 {
                            recorded.fetch_add(1, Ordering::Relaxed);
                            MetadataFetchOutcome::Fetched(("late".to_string(), 1, vec![], 0))
                        } else {
                            MetadataFetchOutcome::SkippedCached
                        };
                        (peer, outcome)
                    }
                },
                &DhtRuntimeStats::default(),
            ),
        )
        .await
        .unwrap();
        actor.await.unwrap();
        assert_eq!(result.unwrap().0.addr.port(), 2000);
        assert_eq!(actual.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn cached_refills_preserve_actual_attempt_and_candidate_scan_bounds() {
        for all_cached in [false, true] {
            let (peer_tx, mut peer_rx) = mpsc::channel(MAX_METADATA_CANDIDATES_PER_HASH);
            drop(peer_tx);
            // Inputs exceed the scan bound; even an all-cached stream must stop at 64.
            let peers = (1000..1100)
                .map(|port| PeerCandidate {
                    addr: SocketAddr::from(([127, 0, 0, 1], port)),
                    discovered_at: Instant::now(),
                })
                .collect();
            let actual = Arc::new(AtomicUsize::new(0));
            let scanned = Arc::new(AtomicUsize::new(0));
            let actual_for_fetch = actual.clone();
            let scanned_for_fetch = scanned.clone();
            let stats = DhtRuntimeStats::default();
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                race_peer_fetches(
                    peers,
                    &mut peer_rx,
                    None,
                    move |peer| {
                        let actual = actual_for_fetch.clone();
                        let scanned = scanned_for_fetch.clone();
                        async move {
                            scanned.fetch_add(1, Ordering::Relaxed);
                            let outcome = if all_cached || peer.addr.port() < 1012 {
                                MetadataFetchOutcome::SkippedCached
                            } else {
                                actual.fetch_add(1, Ordering::Relaxed);
                                tokio::task::yield_now().await;
                                MetadataFetchOutcome::Failed
                            };
                            (peer, outcome)
                        }
                    },
                    &stats,
                ),
            )
            .await
            .unwrap();
            assert!(result.is_none());
            assert_eq!(
                actual.load(Ordering::Relaxed),
                if all_cached {
                    0
                } else {
                    MAX_METADATA_PEERS_PER_HASH
                }
            );
            assert_eq!(
                scanned.load(Ordering::Relaxed),
                if all_cached {
                    MAX_METADATA_CANDIDATES_PER_HASH
                } else {
                    MAX_METADATA_PEERS_PER_HASH * 2
                }
            );
            assert_eq!(
                stats.snapshot().metadata_peer_candidates,
                scanned.load(Ordering::Relaxed) as u64
            );
        }
    }

    #[tokio::test]
    async fn full_lookup_request_queue_ends_wait_promptly() {
        let (_peer_tx, mut peer_rx) = mpsc::channel(12);
        let (lookup, _request_rx) = lookup_request_channel(1);
        let (peer_tx, _result_rx) = mpsc::channel(12);
        lookup
            .request_tx
            .try_send(PeerLookupRequest {
                info_hash: [4; 20],
                peer_tx,
                source_nodes: Vec::new(),
                source: HashSource::GetPeers,
            })
            .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            race_peer_fetches(
                Vec::new(),
                &mut peer_rx,
                Some(lookup),
                |peer| async move { (peer, MetadataFetchOutcome::Failed) },
                &DhtRuntimeStats::default(),
            ),
        )
        .await
        .expect("rejected lookup must not wait for its full deadline");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn finished_lookup_with_no_peers_ends_wait_promptly() {
        let (peer_tx, mut peer_rx) = mpsc::channel(12);
        drop(peer_tx);
        let (lookup, mut request_rx) = lookup_request_channel(1);
        let actor = tokio::spawn(async move {
            drop(request_rx.recv().await.unwrap());
        });
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            race_peer_fetches(
                Vec::new(),
                &mut peer_rx,
                Some(lookup),
                |peer| async move { (peer, MetadataFetchOutcome::Failed) },
                &DhtRuntimeStats::default(),
            ),
        )
        .await
        .unwrap();
        actor.await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn stalled_lookup_is_bounded_and_closes_its_result_stream() {
        let (peer_tx, mut peer_rx) = mpsc::channel(12);
        drop(peer_tx);
        let (lookup, mut request_rx) = lookup_request_channel(1);
        let actor = tokio::spawn(async move {
            let request = request_rx.recv().await.unwrap();
            request.peer_tx.closed().await;
        });
        let result = tokio::time::timeout(
            Duration::from_secs(4),
            race_peer_fetches(
                Vec::new(),
                &mut peer_rx,
                Some(lookup),
                |peer| async move { (peer, MetadataFetchOutcome::Failed) },
                &DhtRuntimeStats::default(),
            ),
        )
        .await
        .expect("a stalled actor must not retain the job indefinitely");
        assert!(result.is_none());
        actor.await.unwrap();
    }

    #[tokio::test]
    async fn metadata_success_cancels_a_lookup_already_in_progress() {
        let (_peer_tx, mut peer_rx) = mpsc::channel(12);
        let (lookup, mut request_rx) = lookup_request_channel(1);
        let actor_started = Arc::new(tokio::sync::Notify::new());
        let started_for_actor = actor_started.clone();
        let actor = tokio::spawn(async move {
            let request = request_rx.recv().await.unwrap();
            started_for_actor.notify_one();
            request.peer_tx.closed().await;
        });
        let initial = PeerCandidate {
            addr: SocketAddr::from(([127, 0, 0, 1], 1000)),
            discovered_at: Instant::now(),
        };
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            race_peer_fetches(
                vec![initial],
                &mut peer_rx,
                Some(lookup),
                move |peer| {
                    let actor_started = actor_started.clone();
                    async move {
                        actor_started.notified().await;
                        (
                            peer,
                            MetadataFetchOutcome::Fetched(("winner".to_string(), 1, Vec::new(), 0)),
                        )
                    }
                },
                &DhtRuntimeStats::default(),
            ),
        )
        .await
        .unwrap();
        assert!(result.is_some());
        tokio::time::timeout(Duration::from_secs(1), actor)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn full_queue_evicts_oldest_for_newer_hash() {
        let start = Instant::now();
        let mut queue = PendingHashQueue::new(2, Duration::from_secs(60));
        queue.push(event("old", 1000, start), start);
        queue.push(
            event("middle", 1001, start + Duration::from_secs(1)),
            start + Duration::from_secs(1),
        );
        let result = queue.push(
            event("new", 1002, start + Duration::from_secs(2)),
            start + Duration::from_secs(2),
        );

        assert_eq!(result, QueuePushKind::EvictedOldest);
        assert!(!queue.contains("old"));
        assert!(queue.contains("middle"));
        assert!(queue.contains("new"));
    }

    #[test]
    fn queue_expires_stale_hashes_and_pops_newest_first() {
        let start = Instant::now();
        let mut queue = PendingHashQueue::new(4, Duration::from_secs(10));
        queue.push(event("older", 1000, start), start);
        queue.push(
            event("newer", 1001, start + Duration::from_secs(1)),
            start + Duration::from_secs(1),
        );

        let in_flight = HashMap::new();
        assert!(
            queue
                .pop_newest_ready(start + Duration::from_millis(24), &in_flight)
                .is_none()
        );
        let newest = queue
            .pop_newest_ready(start + Duration::from_secs(2), &in_flight)
            .unwrap();
        assert_eq!(newest.info_hash, "newer");
        assert_eq!(queue.expire(start + Duration::from_secs(11)), 1);
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn gate_rejection_does_not_emit_completion() {
        let (hash_tx, hash_rx) = mpsc::channel(4);
        let gate_calls = Arc::new(AtomicUsize::new(0));
        let gate_calls_for_callback = gate_calls.clone();
        let gate = fetch_gate(move |_| {
            gate_calls_for_callback.fetch_add(1, Ordering::Relaxed);
            async { false }
        });
        let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
        let completion = completion_callback(move |result| {
            let _ = completion_tx.send(result);
        });
        let shutdown = CancellationToken::new();
        let scheduler = MetadataScheduler::new(
            hash_rx,
            Arc::new(RbitFetcher::new(1)),
            MetadataSchedulerLimits {
                queue_size: 4,
                concurrency: 1,
            },
            MetadataSchedulerCallbacks {
                torrent: empty_torrent_callback(),
                fetch_gate: gate,
                completion,
            },
            Arc::new(AtomicUsize::new(0)),
            shutdown.clone(),
        );
        let scheduler_task = tokio::spawn(scheduler.run());
        hash_tx
            .send(event("bad", 1000, Instant::now()))
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(1), async {
            while gate_calls.load(Ordering::Relaxed) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), completion_rx.recv())
                .await
                .is_err()
        );

        shutdown.cancel();
        scheduler_task.await.unwrap();
    }

    #[tokio::test]
    async fn admitted_failure_emits_one_completion() {
        let (hash_tx, hash_rx) = mpsc::channel(4);
        let gate = fetch_gate(|_| async { true });
        let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
        let completion = completion_callback(move |result| {
            let _ = completion_tx.send(result);
        });
        let shutdown = CancellationToken::new();
        let scheduler = MetadataScheduler::new(
            hash_rx,
            Arc::new(RbitFetcher::new(1)),
            MetadataSchedulerLimits {
                queue_size: 4,
                concurrency: 1,
            },
            MetadataSchedulerCallbacks {
                torrent: empty_torrent_callback(),
                fetch_gate: gate,
                completion,
            },
            Arc::new(AtomicUsize::new(0)),
            shutdown.clone(),
        );
        let scheduler_task = tokio::spawn(scheduler.run());
        hash_tx
            .send(event("bad", 1000, Instant::now()))
            .await
            .unwrap();

        let result = tokio::time::timeout(Duration::from_secs(1), completion_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.info_hash, "bad");
        assert_eq!(result.status, MetadataFetchCompletionStatus::FetchFailed);
        assert_eq!(result.attempts, 0);
        assert!(
            tokio::time::timeout(Duration::from_millis(50), completion_rx.recv())
                .await
                .is_err()
        );

        shutdown.cancel();
        scheduler_task.await.unwrap();
    }

    #[test]
    fn retries_have_fixed_lifetime_bounded_history_and_exponential_failure_budget() {
        let now = Instant::now();
        let policy = MetadataOptions {
            retry_initial_delay_secs: 1,
            retry_max_delay_secs: 3,
            retry_max_attempts: 3,
            retry_history_ttl_secs: 20,
            retry_history_capacity: 1,
            ..MetadataOptions::default()
        };
        let mut history = RetryHistory::default();
        let first = history
            .record("h", now, true, Duration::ZERO, &policy)
            .unwrap();
        assert_eq!(first.failures, 1);
        assert_eq!(first.next_due, now + Duration::from_secs(1));
        let deferred = history
            .record(
                "h",
                now + Duration::from_secs(1),
                false,
                Duration::from_secs(5),
                &policy,
            )
            .unwrap();
        assert_eq!(
            deferred.failures, 1,
            "lease checks are not download attempts"
        );
        assert_eq!(deferred.expires_at, first.expires_at);
        let second = history
            .record(
                "h",
                now + Duration::from_secs(6),
                true,
                Duration::ZERO,
                &policy,
            )
            .unwrap();
        assert_eq!(second.next_due, now + Duration::from_secs(8));
        let third = history
            .record(
                "h",
                now + Duration::from_secs(8),
                true,
                Duration::ZERO,
                &policy,
            )
            .unwrap();
        assert_eq!(third.next_due, now + Duration::from_secs(11));
        assert_eq!(third.failures, policy.retry_max_attempts);
        assert!(
            history
                .record("other", now, true, Duration::ZERO, &policy)
                .is_none()
        );
        history.expire(now + Duration::from_secs(20));
        assert!(history.entries.is_empty());
        assert!(history.expiry.is_empty());
    }

    #[test]
    fn deferred_candidates_are_preserved_without_sliding_due_time_or_unbounded_indexes() {
        let now = Instant::now();
        let mut queue = PendingHashQueue::new(8, HASH_QUEUE_TTL);
        queue.push(event("h", 1000, now), now);
        let entry = queue.remove("h").unwrap();
        let record = RetryRecord {
            failures: 1,
            extra_attempts: 0,
            defers: 0,
            next_due: now + Duration::from_secs(2),
            expires_at: now + Duration::from_secs(10),
        };
        assert!(queue.defer(entry, record));
        for _ in 0..1_000 {
            queue.push(
                event("h", 1001, now + Duration::from_secs(1)),
                now + Duration::from_secs(1),
            );
        }
        assert_eq!(queue.waiting.len(), 1);
        assert_eq!(queue.expiry.len(), 1);
        assert_eq!(queue.retry_expiry.len(), 1);
        assert_eq!(
            queue.pressure_len(),
            0,
            "cooling work must not slow discovery"
        );
        assert!(
            queue
                .pop_newest_ready(now + Duration::from_secs(1), &HashMap::new())
                .is_none()
        );
        let retained = queue
            .pop_newest_ready(record.next_due, &HashMap::new())
            .unwrap();
        assert_eq!(retained.peers.len(), 2);
        assert_eq!(retained.peers[0].addr.port(), 1001);
        assert_eq!(retained.expires_at, record.expires_at);
        assert!(
            queue.expiry.is_empty() && queue.waiting.is_empty() && queue.retry_expiry.is_empty()
        );
    }

    #[test]
    fn fresh_and_due_retry_work_have_bounded_fairness_and_cold_retries_reserve_no_pressure() {
        let now = Instant::now();
        let mut queue = PendingHashQueue::new(12, HASH_QUEUE_TTL);
        queue.push(event("retry", 1000, now), now);
        let retry = queue.remove("retry").unwrap();
        assert!(queue.defer(
            retry,
            RetryRecord {
                failures: 1,
                extra_attempts: 0,
                defers: 0,
                next_due: now + Duration::from_secs(1),
                expires_at: now + Duration::from_secs(10)
            }
        ));
        for offset in 0..6 {
            queue.push(event(&format!("fresh-{offset}"), 1100 + offset, now), now);
        }
        let kinds: Vec<_> = (0..4)
            .map(|_| {
                queue
                    .pop_newest_ready(now + Duration::from_secs(1), &HashMap::new())
                    .unwrap()
                    .is_retry
            })
            .collect();
        assert_eq!(kinds, [false, false, false, true]);
    }

    #[tokio::test]
    async fn hash_only_job_uses_source_for_lookup_without_connecting_to_its_udp_address() {
        let source = NodeTuple {
            id: [8; 20],
            addr: SocketAddr::from(([127, 0, 0, 1], 9999)),
        };
        let (request_tx, mut request_rx) = mpsc::channel(1);
        let actor = tokio::spawn(async move {
            let request = request_rx.recv().await.unwrap();
            let request: PeerLookupRequest = request;
            assert_eq!(request.source_nodes[0].addr, source.addr);
            // An empty lookup must never turn its seed endpoint into a TCP candidate.
        });
        let (peer_tx, peer_rx) = mpsc::channel(1);
        drop(peer_tx);
        let stats = DhtRuntimeStats::default();
        let result = MetadataScheduler::process_hash(
            MetadataJob {
                info_hash: hex::encode([3; 20]),
                peers: Vec::new(),
                peer_rx,
                source_nodes: vec![source],
                source: HashSource::GetPeers,
            },
            &Arc::new(RbitFetcher::new(1)),
            &empty_torrent_callback(),
            &fetch_gate(|_| async { true }),
            &stats,
            Some(request_tx),
            Duration::from_secs(1),
        )
        .await;
        actor.await.unwrap();
        assert_eq!(result.attempts, 0);
        assert_eq!(stats.snapshot().metadata_peer_attempts, 0);
        assert_eq!(
            stats.yield_stats().snapshot()["metadata_hash_only_started"],
            1
        );
    }

    #[tokio::test]
    async fn one_new_peer_during_failure_cooldown_is_fetched_without_another_announce() {
        use crate::metadata::wire::MetadataMsg;
        use crate::metadata::wire::test_support::{
            TestPeerConn, encode_ext_handshake, encode_metadata_data,
        };
        use bytes::Bytes;
        use sha1::{Digest, Sha1};
        use tokio::net::TcpListener;
        let metadata = Bytes::from_static(b"d6:lengthi1e4:name1:xe");
        let hash: [u8; 20] = Sha1::digest(&metadata).into();
        let failed_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let failed_addr = failed_listener.local_addr().unwrap();
        let good_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let good_addr = good_listener.local_addr().unwrap();
        drop(failed_listener);
        let admissions = Arc::new(AtomicUsize::new(0));
        let seen_admissions = admissions.clone();
        let gate: Arc<ArcSwapOption<MetadataFetchCallback>> = Arc::new(ArcSwapOption::empty());
        gate.store(Some(Arc::new(Box::new(move |_| {
            let number = seen_admissions.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                if number == 1 {
                    MetadataAdmission::RetryAfter(Duration::from_millis(200))
                } else {
                    MetadataAdmission::Fetch
                }
            })
        }))));
        let check_admissions = admissions.clone();
        let peer_actor = tokio::spawn(async move {
            let (stream, _) = good_listener.accept().await.unwrap();
            assert!(
                check_admissions.load(Ordering::Relaxed) >= 3,
                "new peers must still pass admission after cooldown"
            );
            let mut conn = TestPeerConn::accept(stream, hash, [9; 20]).await.unwrap();
            assert_eq!(conn.receive_extended().await.unwrap().0, 0);
            conn.send_extended(0, &encode_ext_handshake(Some(3), Some(metadata.len() as i64)))
                .await
                .unwrap();
            let (id, payload) = conn.receive_extended().await.unwrap();
            assert_eq!(id, 3);
            assert!(matches!(
                MetadataMsg::decode(payload).unwrap(),
                MetadataMsg::Request { .. }
            ));
            conn.send_extended(1, &encode_metadata_data(0, metadata.len() as u64, &metadata))
                .await
                .unwrap();
        });
        let torrent: Arc<ArcSwapOption<TorrentAckCallback>> = Arc::new(ArcSwapOption::empty());
        torrent.store(Some(Arc::new(Box::new(|_| true))));
        let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
        let (hash_tx, hash_rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let stats = DhtRuntimeStats::default();
        let scheduler = MetadataScheduler::new_with_runtime(
            hash_rx,
            Arc::new(RbitFetcher::new(1)),
            MetadataSchedulerLimits {
                queue_size: 32,
                concurrency: 1,
            },
            MetadataSchedulerCallbacks {
                torrent,
                fetch_gate: gate,
                completion: completion_callback(move |result| {
                    let _ = completion_tx.send(result);
                }),
            },
            Arc::new(AtomicUsize::new(0)),
            shutdown.clone(),
            MetadataSchedulerRuntime {
                stats: stats.clone(),
                peer_lookup_tx: None,
                lookup_timeout: Duration::from_secs(1),
                metadata_options: MetadataOptions {
                    retry_initial_delay_secs: 0,
                    retry_max_delay_secs: 0,
                    ..MetadataOptions::default()
                },
            },
        );
        let task = tokio::spawn(scheduler.run());
        hash_tx
            .send(HashDiscovered {
                info_hash: hex::encode(hash),
                peer_addr: Some(failed_addr),
                source_node: None,
                source: HashSource::Announce,
                discovered_at: Instant::now(),
            })
            .await
            .unwrap();
        let first = tokio::time::timeout(Duration::from_secs(2), completion_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.status, MetadataFetchCompletionStatus::FetchFailed);
        hash_tx
            .send(HashDiscovered {
                info_hash: hex::encode(hash),
                peer_addr: Some(good_addr),
                source_node: None,
                source: HashSource::Announce,
                discovered_at: Instant::now(),
            })
            .await
            .unwrap();
        let second = tokio::time::timeout(Duration::from_secs(2), completion_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.status, MetadataFetchCompletionStatus::Accepted);
        assert_eq!(admissions.load(Ordering::Relaxed), 3);
        assert_eq!(
            first.attempts + second.attempts,
            2,
            "the cached failed endpoint must not cause another TCP attempt"
        );
        assert_eq!(stats.yield_stats().snapshot()["metadata_retry_deferred"], 1);
        shutdown.cancel();
        task.await.unwrap();
        peer_actor.await.unwrap();
    }

    #[test]
    fn exhausted_history_allows_only_two_new_candidate_chances_and_never_old_duplicates() {
        let now = Instant::now();
        let policy = MetadataOptions {
            retry_max_attempts: 1,
            ..MetadataOptions::default()
        };
        let mut history = RetryHistory::default();
        history
            .record("h", now, true, Duration::ZERO, &policy)
            .unwrap();
        let mut queue = PendingHashQueue::new(8, HASH_QUEUE_TTL);
        queue.push(event("h", 1000, now), now);
        history.remember(&queue.remove("h").unwrap());
        for _ in 0..100 {
            let record = history
                .observe_event(&event("h", 1000, now), now, &policy)
                .unwrap();
            assert_eq!(record.extra_attempts, 0);
            assert_eq!(record.failures, record.attempt_limit(&policy));
        }
        for (offset, expected_extra) in [(1, 1), (2, 2), (3, 2)] {
            let record = history
                .observe_event(&event("h", 1000 + offset, now), now, &policy)
                .unwrap();
            assert_eq!(record.extra_attempts, expected_extra);
            if offset <= 2 {
                assert!(record.failures < record.attempt_limit(&policy));
                history
                    .record("h", now, true, Duration::ZERO, &policy)
                    .unwrap();
            } else {
                assert_eq!(record.failures, record.attempt_limit(&policy));
            }
        }
    }

    #[test]
    fn retry_extreme_durations_are_clamped_without_panics() {
        let now = Instant::now();
        let policy = MetadataOptions {
            retry_initial_delay_secs: u64::MAX,
            retry_max_delay_secs: u64::MAX,
            retry_history_ttl_secs: u64::MAX,
            ..MetadataOptions::default()
        };
        let mut history = RetryHistory::default();
        let record = history
            .record("h", now, true, Duration::ZERO, &policy)
            .unwrap();
        assert_eq!(record.next_due.duration_since(now), MAX_RETRY_LIFETIME);
        assert_eq!(record.expires_at.duration_since(now), MAX_RETRY_LIFETIME);
        let deferred = history
            .record("h", now, false, Duration::MAX, &policy)
            .unwrap();
        assert_eq!(deferred.next_due.duration_since(now), MAX_RETRY_LIFETIME);
    }

    #[tokio::test]
    async fn panicking_admission_releases_running_entry_and_disabled_retry_keeps_new_announces_eligible()
     {
        let admissions = Arc::new(AtomicUsize::new(0));
        let called = admissions.clone();
        let gate = fetch_gate(move |_| {
            let call = called.fetch_add(1, Ordering::Relaxed);
            async move {
                assert_ne!(call, 0, "test admission panic");
                false
            }
        });
        let (hash_tx, hash_rx) = mpsc::channel(8);
        let (completion_tx, mut completion_rx) = mpsc::unbounded_channel();
        let shutdown = CancellationToken::new();
        let stats = DhtRuntimeStats::default();
        let scheduler = MetadataScheduler::new_with_runtime(
            hash_rx,
            Arc::new(RbitFetcher::new(1)),
            MetadataSchedulerLimits {
                queue_size: 8,
                concurrency: 1,
            },
            MetadataSchedulerCallbacks {
                torrent: empty_torrent_callback(),
                fetch_gate: gate,
                completion: completion_callback(move |result| {
                    let _ = completion_tx.send(result);
                }),
            },
            Arc::new(AtomicUsize::new(0)),
            shutdown.clone(),
            MetadataSchedulerRuntime {
                stats: stats.clone(),
                peer_lookup_tx: None,
                lookup_timeout: Duration::from_secs(1),
                metadata_options: MetadataOptions {
                    retry_max_attempts: 0,
                    ..MetadataOptions::default()
                },
            },
        );
        let task = tokio::spawn(scheduler.run());
        hash_tx
            .send(event("h", 1000, Instant::now()))
            .await
            .unwrap();
        let completion = tokio::time::timeout(Duration::from_secs(1), completion_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            completion.status,
            MetadataFetchCompletionStatus::FetchFailed
        );
        hash_tx
            .send(event("h", 1001, Instant::now()))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while admissions.load(Ordering::Relaxed) < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            stats.yield_stats().snapshot()["metadata_retry_scheduled"],
            0
        );
        shutdown.cancel();
        task.await.unwrap();
        assert_eq!(stats.snapshot().metadata_in_flight, 0);
    }

    #[tokio::test]
    async fn hash_only_and_cached_initial_peers_start_lookup_without_100ms_delay() {
        // Case 1: hash-only job (no initial peers) dispatches lookup immediately (<50ms).
        let (_peer_tx, mut peer_rx) = mpsc::channel(4);
        let (lookup, mut request_rx) = lookup_request_channel(1);
        let started = Instant::now();
        let driver = tokio::spawn(async move {
            let _req = tokio::time::timeout(Duration::from_millis(50), request_rx.recv())
                .await
                .expect("hash-only lookup must not wait 100ms")
                .unwrap();
        });
        let result = race_peer_fetches(
            Vec::new(),
            &mut peer_rx,
            Some(lookup),
            |peer| async move { (peer, MetadataFetchOutcome::Failed) },
            &DhtRuntimeStats::default(),
        )
        .await;
        assert!(result.is_none());
        driver.await.unwrap();
        let _ = started;

        // Case 2: cached initial peer skips immediately (<50ms) -> lookup dispatches immediately.
        let (_peer_tx2, mut peer_rx2) = mpsc::channel(4);
        let (lookup2, mut request_rx2) = lookup_request_channel(1);
        let driver2 = tokio::spawn(async move {
            let _req = tokio::time::timeout(Duration::from_millis(50), request_rx2.recv())
                .await
                .expect("all-cached initial peers must trigger lookup without waiting 100ms")
                .unwrap();
        });
        let initial = vec![PeerCandidate {
            addr: SocketAddr::from(([127, 0, 0, 1], 1000)),
            discovered_at: Instant::now(),
        }];
        let result2 = race_peer_fetches(
            initial,
            &mut peer_rx2,
            Some(lookup2),
            |peer| async move { (peer, MetadataFetchOutcome::SkippedCached) },
            &DhtRuntimeStats::default(),
        )
        .await;
        assert!(result2.is_none());
        driver2.await.unwrap();
    }

    #[test]
    fn repeated_defers_back_off_and_stop_requeuing_after_limit() {
        let now = Instant::now();
        let policy = MetadataOptions {
            retry_initial_delay_secs: 62,
            retry_max_delay_secs: 3_600,
            retry_max_attempts: 5,
            retry_history_ttl_secs: 21_600,
            retry_history_capacity: 100,
            ..MetadataOptions::default()
        };
        let mut history = RetryHistory::default();
        let delay = Duration::from_secs(5);
        for attempt in 1..=MAX_DEFER_RECHECKS {
            let record = history.record("h", now, false, delay, &policy).unwrap();
            assert_eq!(record.failures, 0);
            assert_eq!(record.defers, attempt);
            let expected = match attempt {
                1 => Duration::from_secs(5),
                2 => Duration::from_secs(10),
                3 => Duration::from_secs(20),
                4 => Duration::from_secs(40),
                _ => Duration::from_secs(60),
            };
            assert_eq!(record.next_due.duration_since(now), expected);
        }

        let (_hash_tx, hash_rx) = mpsc::channel(4);
        let stats = DhtRuntimeStats::default();
        let scheduler = MetadataScheduler::new_with_runtime(
            hash_rx,
            Arc::new(RbitFetcher::new(1)),
            MetadataSchedulerLimits {
                queue_size: 16,
                concurrency: 1,
            },
            MetadataSchedulerCallbacks {
                torrent: empty_torrent_callback(),
                fetch_gate: Arc::new(ArcSwapOption::empty()),
                completion: Arc::new(ArcSwapOption::empty()),
            },
            Arc::new(AtomicUsize::new(0)),
            CancellationToken::new(),
            MetadataSchedulerRuntime {
                stats: stats.clone(),
                peer_lookup_tx: None,
                metadata_options: policy.clone(),
                lookup_timeout: Duration::from_secs(1),
            },
        );
        let mut queue = PendingHashQueue::new(16, HASH_QUEUE_TTL);
        queue.push(event("h", 1000, now), now);
        let entry = queue.remove("h").unwrap();
        for _ in 0..MAX_DEFER_RECHECKS {
            scheduler.schedule_retry(entry.clone(), &mut queue, false, delay);
        }
        assert!(queue.contains("h"));
        // Exceeding MAX_DEFER_RECHECKS removes it from retry queue and marks exhausted.
        let entry = queue.remove("h").unwrap();
        scheduler.schedule_retry(entry, &mut queue, false, delay);
        assert!(!queue.contains("h"));
        assert!(stats.yield_stats().snapshot()["metadata_retry_exhausted"] >= 1);
    }

    #[test]
    fn full_retry_quota_does_not_evict_fresh_entries_or_silently_drop() {
        let (_hash_tx, hash_rx) = mpsc::channel(4);
        let stats = DhtRuntimeStats::default();
        let scheduler = MetadataScheduler::new_with_runtime_stats(
            hash_rx,
            Arc::new(RbitFetcher::new(1)),
            MetadataSchedulerLimits {
                queue_size: 4,
                concurrency: 1,
            },
            MetadataSchedulerCallbacks {
                torrent: empty_torrent_callback(),
                fetch_gate: Arc::new(ArcSwapOption::empty()),
                completion: Arc::new(ArcSwapOption::empty()),
            },
            Arc::new(AtomicUsize::new(0)),
            CancellationToken::new(),
            stats.clone(),
        );
        let now = Instant::now();
        let mut queue = PendingHashQueue::new(4, HASH_QUEUE_TTL);
        // Retry quota is capacity / 4 = 1. Fill it with "retry-1", plus a fresh entry.
        queue.push(event("retry-1", 1000, now), now);
        let retry_entry = queue.remove("retry-1").unwrap();
        scheduler.schedule_retry(retry_entry, &mut queue, true, Duration::ZERO);
        queue.push(event("fresh-1", 1001, now), now);
        assert_eq!(queue.retry_expiry.len(), 1);
        assert_eq!(queue.len(), 2);

        // Record another retry hash "retry-2" in retry_history, then receive an event for it.
        {
            let mut history = scheduler.retry_history.lock().unwrap();
            history.record(
                "retry-2",
                now,
                true,
                Duration::ZERO,
                &scheduler.metadata_options,
            );
        }
        scheduler.enqueue(&mut queue, &mut HashMap::new(), event("retry-2", 1002, now));

        assert!(queue.contains("fresh-1"));
        assert!(queue.contains("retry-1"));
        assert!(!queue.contains("retry-2"));
        assert_eq!(stats.yield_stats().snapshot()["metadata_retry_evicted"], 1);
        assert_eq!(stats.snapshot().metadata_queue_inserted, 0);
    }

    #[test]
    fn fresh_live_peer_wakes_long_backoff_and_extra_attempt_promptly() {
        let now = Instant::now();
        let policy = MetadataOptions {
            retry_initial_delay_secs: 60,
            retry_max_delay_secs: 600,
            retry_max_attempts: 2,
            ..MetadataOptions::default()
        };
        let mut history = RetryHistory::default();
        // Two failures -> second backoff is 120s (> retry_initial_delay_secs = 60s), exhausted.
        history.record("h", now, true, Duration::ZERO, &policy);
        let second = history.record("h", now, true, Duration::ZERO, &policy).unwrap();
        assert_eq!(second.next_due, now + Duration::from_secs(120));

        // Exhausted + brand new peer -> wakes immediately (100ms), not 60s.
        let woken = history
            .observe_event(&event("h", 2000, now), now, &policy)
            .unwrap();
        assert_eq!(woken.extra_attempts, 1);
        assert_eq!(woken.next_due, now + Duration::from_millis(100));

        // Now test non-exhausted long backoff (> 60s).
        let policy3 = MetadataOptions {
            retry_max_attempts: 5,
            ..policy
        };
        history.record("h2", now, true, Duration::ZERO, &policy3);
        let long = history.record("h2", now, true, Duration::ZERO, &policy3).unwrap();
        assert_eq!(long.next_due, now + Duration::from_secs(120));
        let pulled = history
            .observe_event(&event("h2", 2001, now), now, &policy3)
            .unwrap();
        assert_eq!(pulled.next_due, now + Duration::from_millis(100));
    }
