use super::*;
use crate::metadata::bencode::{BencodeValue, encode};
use crate::metadata::wire::test_support::{
    TestPeerConn, encode_ext_handshake, encode_metadata_data, encode_metadata_reject,
};
use crate::{MetadataConnectionErrorsSnapshot, MetadataTimeoutStagesSnapshot};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn serve_metadata(mut conn: TestPeerConn, metadata: Bytes, reject: bool) {
    let (id, _) = conn.receive_extended().await.unwrap();
    assert_eq!(id, 0);
    conn.send_extended(0, &encode_ext_handshake(Some(3), Some(metadata.len() as i64)))
        .await
        .unwrap();
    let (ext_id, payload) = conn.receive_extended().await.unwrap();
    assert_eq!(ext_id, 3, "expected metadata request using the remote extension ID");
    assert_eq!(
        MetadataMsg::decode(payload).unwrap(),
        MetadataMsg::Request { piece: 0 }
    );
    let response = if reject {
        encode_metadata_reject(0)
    } else {
        encode_metadata_data(0, metadata.len() as u64, &metadata)
    };
    conn.send_extended(1, &response).await.unwrap();
    if reject {
        // Keep the TCP socket open: success requires explicit Reject handling, not EOF.
        std::future::pending::<()>().await;
    }
}

#[tokio::test]
async fn torrent_failures_do_not_block_other_hashes_at_the_same_endpoint() {
    let metadata = Bytes::from_static(b"d6:lengthi1e4:name1:xe");
    let good_hash: [u8; 20] = Sha1::digest(&metadata).into();
    for failure in [
        "hash_mismatch",
        "reject",
        "piece_timeout",
        "handshake_timeout",
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let metadata_for_peer = metadata.clone();
        let actor = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let bad_metadata = metadata_for_peer.clone();
            let first = tokio::spawn(async move {
                match failure {
                    "hash_mismatch" => {
                        let mut request = [0; 68];
                        stream.read_exact(&mut request).await.unwrap();
                        stream
                            .write_all(&Handshake::new(good_hash, [9; 20]).encode())
                            .await
                            .unwrap();
                    }
                    "handshake_timeout" => {
                        let _stream = stream;
                        std::future::pending::<()>().await;
                    }
                    _ => {
                        let mut conn = TestPeerConn::accept(stream, [7; 20], [9; 20])
                            .await
                            .unwrap();
                        if failure == "reject" {
                            serve_metadata(conn, bad_metadata, true).await;
                        } else {
                            conn.receive_extended().await.unwrap();
                            conn.send_extended(0, &encode_ext_handshake(Some(3), Some(1)))
                                .await
                                .unwrap();
                            conn.receive_extended().await.unwrap();
                            std::future::pending::<()>().await;
                        }
                    }
                }
            });
            let (stream, _) = listener.accept().await.unwrap();
            let conn = TestPeerConn::accept(stream, good_hash, [9; 20])
                .await
                .unwrap();
            serve_metadata(conn, metadata_for_peer, false).await;
            first.abort();
            let _ = first.await;
        });
        let stats = DhtRuntimeStats::default();
        let fetcher = RbitFetcher::new_with_runtime_stats(1, 10, 60, stats.clone());
        let started = Instant::now();
        assert!(
            matches!(
                fetcher.fetch(&[7; 20], addr).await,
                MetadataFetchOutcome::Failed
            ),
            "{failure}"
        );
        if failure == "reject" {
            assert!(
                started.elapsed() < Duration::from_millis(800),
                "explicit rejection must not wait for timeout"
            );
            assert_eq!(stats.observability_snapshot().metadata_failure_reject, 1);
            assert_eq!(stats.snapshot().metadata_peer_timeouts, 0);
        }
        assert!(
            matches!(
                fetcher.fetch(&[7; 20], addr).await,
                MetadataFetchOutcome::SkippedCached
            ),
            "same hash must remain cooled down: {failure}"
        );
        assert!(
            matches!(
                fetcher.fetch(&good_hash, addr).await,
                MetadataFetchOutcome::Fetched(_)
            ),
            "unrelated hash must remain reachable: {failure}"
        );
        timeout(Duration::from_secs(2), actor)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stats.snapshot().metadata_peer_attempts, 2);
        assert_eq!(stats.observability_snapshot().peer_cache_metadata_hits, 1);
        assert_eq!(stats.observability_snapshot().peer_cache_endpoint_hits, 0);
    }
}

#[tokio::test]
async fn unreachable_tcp_endpoint_is_cooled_down_across_hashes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let stats = DhtRuntimeStats::default();
    let fetcher = RbitFetcher::new_with_runtime_stats(1, 10, 60, stats.clone());
    assert!(matches!(
        fetcher.fetch(&[7; 20], addr).await,
        MetadataFetchOutcome::Failed
    ));
    assert!(matches!(
        fetcher.fetch(&[8; 20], addr).await,
        MetadataFetchOutcome::SkippedCached
    ));
    let obs = stats.observability_snapshot();
    // Windows may deliver a refused-loopback error after our deadline; either
    // outcome must remain TCP-only and use endpoint scope.
    assert_eq!(
        obs.metadata_failure_tcp_connect + obs.metadata_timeout_stages.tcp_connect,
        1
    );
    assert_eq!(obs.metadata_failure_base_handshake, 0);
    assert_eq!(obs.peer_cache_endpoint_hits, 1);
    assert_eq!(obs.peer_cache_metadata_hits, 0);
}

#[test]
fn validation_pool_delay_does_not_cool_down_a_healthy_peer() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            ready_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        ready_rx.await.unwrap();
        let metadata = Bytes::from_static(b"d6:lengthi1e4:name1:xe");
        let hash: [u8; 20] = Sha1::digest(&metadata).into();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let actor = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let conn = TestPeerConn::accept(stream, hash, [9; 20]).await.unwrap();
                serve_metadata(conn, metadata.clone(), false).await;
            }
        });
        let stats = DhtRuntimeStats::default();
        let fetcher = RbitFetcher::new_with_runtime_stats(1, 10, 60, stats.clone());
        let first = fetcher.fetch(&hash, addr).await;
        // Release the blocking pool before asserting to keep failures from hanging runtime drop.
        drop(release_tx);
        blocker.await.unwrap();
        assert!(matches!(first, MetadataFetchOutcome::Failed));
        assert_eq!(
            stats
                .observability_snapshot()
                .metadata_timeout_stages
                .validation,
            1
        );
        assert_eq!(stats.snapshot().metadata_peer_failure_cache_entries, 0);
        assert!(matches!(
            fetcher.fetch(&hash, addr).await,
            MetadataFetchOutcome::Fetched(_)
        ));
        timeout(Duration::from_secs(2), actor)
            .await
            .unwrap()
            .unwrap();
    });
}

#[test]
fn peer_failure_cache_is_socket_specific_and_expires() {
    let start = Instant::now();
    let cache = PeerFailureCache::new(10, Duration::from_secs(60));
    let first = PeerFailureKey::Endpoint("127.0.0.1:1000".parse().unwrap());
    let same_ip_other_port = PeerFailureKey::Endpoint("127.0.0.1:1001".parse().unwrap());

    assert_eq!(cache.insert(first, PeerFailureReason::Timeout, start), 1);
    assert_eq!(cache.get(first, start).0, Some(PeerFailureReason::Timeout));
    assert_eq!(cache.get(same_ip_other_port, start).0, None);
    assert_eq!(cache.get(first, start + Duration::from_secs(61)), (None, 0));
}

#[test]
fn peer_failure_cache_evicts_oldest_at_capacity() {
    let start = Instant::now();
    let cache = PeerFailureCache::new(1, Duration::from_secs(60));
    let first = PeerFailureKey::Endpoint("127.0.0.1:1000".parse().unwrap());
    let second = PeerFailureKey::Endpoint("127.0.0.1:1001".parse().unwrap());

    cache.insert(first, PeerFailureReason::Timeout, start);
    cache.insert(second, PeerFailureReason::ConnectFailed, start);

    assert_eq!(cache.get(first, start).0, None);
    assert_eq!(
        cache.get(second, start).0,
        Some(PeerFailureReason::ConnectFailed)
    );
}

#[tokio::test]
async fn total_timeout_covers_peer_handshake() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let accept_task = tokio::spawn(async move {
        let (_stream, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });

    let stats = DhtRuntimeStats::default();
    let fetcher = RbitFetcher::new_with_runtime_stats(1, 10, 60, stats.clone());
    let started = Instant::now();
    assert!(matches!(
        fetcher.fetch(&[7; 20], addr).await,
        MetadataFetchOutcome::Failed
    ));
    assert!(started.elapsed() < Duration::from_secs(2));

    let cached_started = Instant::now();
    assert!(matches!(
        fetcher.fetch(&[7; 20], addr).await,
        MetadataFetchOutcome::SkippedCached
    ));
    assert!(cached_started.elapsed() < Duration::from_millis(100));

    let snapshot = stats.snapshot();
    assert_eq!(snapshot.metadata_peer_attempts, 1);
    assert_eq!(snapshot.metadata_peer_failed, 1);
    assert_eq!(snapshot.metadata_peer_timeouts, 1);
    assert_eq!(snapshot.metadata_peer_failure_cache_hits, 1);
    assert_eq!(snapshot.metadata_peer_failure_cache_entries, 1);

    let observability = stats.observability_snapshot();
    assert_eq!(observability.metadata_failure_timeout, 1);
    assert_eq!(observability.metadata_failure_connect, 0);
    assert_eq!(
        observability.metadata_timeout_stages,
        MetadataTimeoutStagesSnapshot {
            connect_handshake: 1,
            base_handshake: 1,
            ..Default::default()
        }
    );

    accept_task.abort();
}

#[tokio::test]
async fn deadlines_distinguish_extension_handshake_from_piece_wait() {
    for request_pieces in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut conn = TestPeerConn::accept(stream, [7; 20], [9; 20])
                .await
                .unwrap();
            assert_eq!(conn.receive_extended().await.unwrap().0, 0);
            if request_pieces {
                conn.send_extended(0, &encode_ext_handshake(Some(3), Some(1)))
                    .await
                    .unwrap();
                let (id, payload) = conn.receive_extended().await.unwrap();
                assert_eq!(id, 3);
                assert!(matches!(
                    MetadataMsg::decode(payload).unwrap(),
                    MetadataMsg::Request { .. }
                ));
            }
            ready_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let stats = DhtRuntimeStats::default();
        let fetcher = RbitFetcher::new_with_runtime_stats(1, 0, 0, stats.clone());
        assert!(matches!(
            fetcher.fetch(&[7; 20], addr).await,
            MetadataFetchOutcome::Failed
        ));
        ready_rx.await.unwrap();
        peer.abort();

        let runtime = stats.snapshot();
        assert_eq!(runtime.metadata_peer_attempts, 1);
        assert_eq!(runtime.metadata_peer_failed, 1);
        assert_eq!(runtime.metadata_peer_timeouts, 1);
        let obs = stats.observability_snapshot();
        assert_eq!(obs.metadata_failure_timeout, 1);
        assert_eq!(obs.metadata_failure_connect, 0);
        assert_eq!(
            obs.metadata_connection_errors,
            MetadataConnectionErrorsSnapshot::default()
        );
        assert_eq!(
            obs.metadata_timeout_stages,
            MetadataTimeoutStagesSnapshot {
                extension_handshake_receive: u64::from(!request_pieces),
                piece_receive: u64::from(request_pieces),
                ..Default::default()
            }
        );
    }
}

#[tokio::test]
async fn base_handshake_errors_preserve_aggregate_connection_failures() {
    let stats = DhtRuntimeStats::default();
    let fetcher = RbitFetcher::new_with_runtime_stats(5, 0, 0, stats.clone());
    // A malformed handshake, a valid handshake for a different hash, and an orderly EOF.
    for response in [
        vec![0; 68],
        Handshake::new([8; 20], [9; 20]).encode().to_vec(),
        vec![],
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut handshake = [0; 68];
            stream.read_exact(&mut handshake).await.unwrap();
            stream.write_all(&response).await.unwrap();
            stream.shutdown().await.unwrap();
        });
        assert!(matches!(
            fetcher.fetch(&[7; 20], addr).await,
            MetadataFetchOutcome::Failed
        ));
        peer.await.unwrap();
    }
    let runtime = stats.snapshot();
    assert_eq!(runtime.metadata_peer_attempts, 3);
    assert_eq!(runtime.metadata_peer_failed, 3);
    assert_eq!(runtime.metadata_connect_failed, 3);
    assert_eq!(runtime.metadata_peer_timeouts, 0);
    let obs = stats.observability_snapshot();
    assert_eq!(obs.metadata_failure_connect, 3);
    assert_eq!(obs.metadata_failure_timeout, 0);
    assert_eq!(
        obs.metadata_timeout_stages,
        MetadataTimeoutStagesSnapshot::default()
    );
    assert_eq!(
        obs.metadata_connection_errors,
        MetadataConnectionErrorsSnapshot {
            invalid_handshake: 1,
            info_hash_mismatch: 1,
            closed: 1,
            ..Default::default()
        }
    );
}

#[test]
fn connection_errors_use_typed_variants_not_error_text() {
    use std::io::Error;
    for (kind, expected) in [
        (ErrorKind::ConnectionRefused, MetadataConnectionError::Refused),
        (ErrorKind::ConnectionReset, MetadataConnectionError::Reset),
        (ErrorKind::ConnectionAborted, MetadataConnectionError::Aborted),
        (ErrorKind::UnexpectedEof, MetadataConnectionError::Closed),
        (ErrorKind::TimedOut, MetadataConnectionError::Timeout),
        (ErrorKind::Other, MetadataConnectionError::IoOther),
    ] {
        assert_eq!(
            connection_error_category(&PeerWireError::Io(Error::new(kind, "opaque"))),
            expected
        );
    }
    assert_eq!(
        connection_error_category(&PeerWireError::InvalidHandshake),
        MetadataConnectionError::InvalidHandshake
    );
    assert_eq!(
        connection_error_category(&PeerWireError::InfoHashMismatch),
        MetadataConnectionError::InfoHashMismatch
    );
    assert_eq!(
        connection_error_category(&PeerWireError::InvalidFrame),
        MetadataConnectionError::Other
    );
    assert_eq!(
        connection_error_category(&PeerWireError::Io(Error::other("timeout"))),
        MetadataConnectionError::IoOther
    );
}

#[tokio::test]
async fn successful_local_metadata_does_not_increment_failure_diagnostics() {
    let metadata = Bytes::from_static(b"d6:lengthi1e4:name1:xe");
    let info_hash: [u8; 20] = Sha1::digest(&metadata).into();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut conn = TestPeerConn::accept(stream, info_hash, [9; 20])
            .await
            .unwrap();
        assert_eq!(conn.receive_extended().await.unwrap().0, 0);
        conn.send_extended(0, &encode_ext_handshake(Some(3), Some(metadata.len() as i64)))
            .await
            .unwrap();
        assert_eq!(conn.receive_extended().await.unwrap().0, 3);
        conn.send_extended(1, &encode_metadata_data(0, metadata.len() as u64, &metadata))
            .await
            .unwrap();
    });
    let stats = DhtRuntimeStats::default();
    let fetcher = RbitFetcher::new_with_runtime_stats(5, 0, 0, stats.clone());
    let MetadataFetchOutcome::Fetched((name, size, files, _)) =
        fetcher.fetch(&info_hash, addr).await
    else {
        panic!("expected successful metadata");
    };
    peer.await.unwrap();
    assert_eq!((name.as_str(), size, files.len()), ("x", 1, 1));
    assert_eq!(stats.snapshot().metadata_peer_succeeded, 1);
    assert_eq!(stats.snapshot().metadata_peer_failed, 0);
    let obs = stats.observability_snapshot();
    assert_eq!(obs.metadata_failure_timeout, 0);
    assert_eq!(obs.metadata_failure_connect, 0);
    assert_eq!(
        obs.metadata_timeout_stages,
        MetadataTimeoutStagesSnapshot::default()
    );
    assert_eq!(
        obs.metadata_connection_errors,
        MetadataConnectionErrorsSnapshot::default()
    );
}

#[tokio::test]
async fn extension_handshake_without_ut_metadata_or_size_fails_fast() {
    for omit_ext in [true, false] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut conn = TestPeerConn::accept(stream, [7; 20], [9; 20])
                .await
                .unwrap();
            assert_eq!(conn.receive_extended().await.unwrap().0, 0);
            let hs = if omit_ext {
                encode_ext_handshake(None, Some(100))
            } else {
                encode_ext_handshake(Some(3), None)
            };
            conn.send_extended(0, &hs).await.unwrap();
            std::future::pending::<()>().await;
        });

        let stats = DhtRuntimeStats::default();
        let fetcher = RbitFetcher::new_with_runtime_stats(5, 0, 0, stats.clone());
        let started = Instant::now();
        assert!(matches!(
            fetcher.fetch(&[7; 20], addr).await,
            MetadataFetchOutcome::Failed
        ));
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "missing ut_metadata/metadata_size must fail fast, not hang 5s"
        );
        assert_eq!(stats.snapshot().metadata_peer_timeouts, 0);
        if omit_ext {
            assert_eq!(stats.snapshot().metadata_no_extension, 1);
        } else {
            assert_eq!(stats.observability_snapshot().metadata_failure_reject, 1);
        }
        peer.abort();
    }
}

#[tokio::test]
async fn oversized_i64_metadata_size_above_u32_max_triggers_size_limit() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut conn = TestPeerConn::accept(stream, [7; 20], [9; 20])
            .await
            .unwrap();
        assert_eq!(conn.receive_extended().await.unwrap().0, 0);
        conn.send_extended(
            0,
            &encode_ext_handshake(Some(3), Some((1i64 << 32) + 100)),
        )
        .await
        .unwrap();
    });

    let stats = DhtRuntimeStats::default();
    let fetcher = RbitFetcher::new_with_runtime_stats(5, 0, 0, stats.clone());
    assert!(matches!(
        fetcher.fetch(&[7; 20], addr).await,
        MetadataFetchOutcome::Failed
    ));
    assert_eq!(
        stats.observability_snapshot().metadata_failure_size_limit,
        1
    );
    peer.await.unwrap();
}

#[tokio::test]
async fn multi_piece_metadata_exceeding_window_downloads_with_sliding_window() {
    let mut info = BTreeMap::new();
    info.insert(Bytes::from_static(b"length"), BencodeValue::Integer(12345));
    let long_name = "a".repeat(320 * 1024);
    info.insert(
        Bytes::from_static(b"name"),
        BencodeValue::string(&long_name),
    );
    info.insert(
        Bytes::from_static(b"piece length"),
        BencodeValue::Integer(16384),
    );
    let full_metadata = encode(&BencodeValue::Dict(info));
    let info_hash: [u8; 20] = Sha1::digest(&full_metadata).into();
    let total_pieces = metadata_piece_count(full_metadata.len());
    assert!(total_pieces > MAX_IN_FLIGHT_PIECE_REQUESTS);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let meta_clone = full_metadata.clone();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut conn = TestPeerConn::accept(stream, info_hash, [9; 20])
            .await
            .unwrap();
        assert_eq!(conn.receive_extended().await.unwrap().0, 0);
        conn.send_extended(0, &encode_ext_handshake(Some(3), Some(meta_clone.len() as i64)))
            .await
            .unwrap();

        for expected_piece in 0..total_pieces {
            let (ext_id, payload) = conn.receive_extended().await.unwrap();
            assert_eq!(ext_id, 3);
            let MetadataMsg::Request { piece } = MetadataMsg::decode(payload).unwrap() else {
                panic!("expected piece request");
            };
            assert_eq!(piece, expected_piece as u32);
            let start = expected_piece * METADATA_PIECE_SIZE;
            let end = (start + METADATA_PIECE_SIZE).min(meta_clone.len());
            conn.send_extended(
                1,
                &encode_metadata_data(piece, meta_clone.len() as u64, &meta_clone[start..end]),
            )
            .await
            .unwrap();
        }
    });

    let stats = DhtRuntimeStats::default();
    let fetcher = RbitFetcher::new_with_runtime_stats(5, 0, 0, stats.clone());
    let MetadataFetchOutcome::Fetched((name, size, _, _)) =
        fetcher.fetch(&info_hash, addr).await
    else {
        panic!("sliding window fetch should succeed");
    };
    assert_eq!(name.len(), long_name.len());
    assert_eq!(size, 12345);
    peer.await.unwrap();
}

#[tokio::test]
async fn invalid_piece_index_or_length_is_rejected_cleanly() {
    let metadata = Bytes::from_static(b"d6:lengthi1e4:name1:xe");
    let info_hash: [u8; 20] = Sha1::digest(&metadata).into();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut conn = TestPeerConn::accept(stream, info_hash, [9; 20])
            .await
            .unwrap();
        assert_eq!(conn.receive_extended().await.unwrap().0, 0);
        conn.send_extended(0, &encode_ext_handshake(Some(3), Some(metadata.len() as i64)))
            .await
            .unwrap();
        assert_eq!(conn.receive_extended().await.unwrap().0, 3);
        conn.send_extended(1, &encode_metadata_data(99, metadata.len() as u64, &metadata))
            .await
            .unwrap();
    });

    let stats = DhtRuntimeStats::default();
    let fetcher = RbitFetcher::new_with_runtime_stats(5, 0, 0, stats.clone());
    assert!(matches!(
        fetcher.fetch(&info_hash, addr).await,
        MetadataFetchOutcome::Failed
    ));
    let obs = stats.observability_snapshot();
    assert_eq!(obs.metadata_failure_protocol, 1);
    assert_eq!(obs.metadata_failure_other, 0);
    peer.await.unwrap();
}

#[tokio::test]
async fn extension_stage_eof_is_classified_as_peer_closed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut conn = TestPeerConn::accept(stream, [7; 20], [9; 20])
            .await
            .unwrap();
        assert_eq!(conn.receive_extended().await.unwrap().0, 0);
        // Drop immediately after receiving our BEP-10 handshake -> clean EOF.
    });

    let stats = DhtRuntimeStats::default();
    let fetcher = RbitFetcher::new_with_runtime_stats(5, 0, 0, stats.clone());
    assert!(matches!(
        fetcher.fetch(&[7; 20], addr).await,
        MetadataFetchOutcome::Failed
    ));
    let obs = stats.observability_snapshot();
    assert_eq!(obs.metadata_failure_peer_closed, 1);
    assert_eq!(obs.metadata_failure_other, 0);
    peer.await.unwrap();
}

#[test]
fn parse_metadata_prefers_utf8_fields_handles_lossy_and_rejects_negative_lengths() {
    let mut f1 = BTreeMap::new();
    f1.insert(Bytes::from_static(b"length"), BencodeValue::Integer(100));
    f1.insert(
        Bytes::from_static(b"path"),
        BencodeValue::List(vec![BencodeValue::Bytes(Bytes::from_static(b"dir/\xff"))]),
    );
    f1.insert(
        Bytes::from_static(b"path.utf-8"),
        BencodeValue::List(vec![
            BencodeValue::string("dir"),
            BencodeValue::string("你好.mp4"),
        ]),
    );
    let mut info = BTreeMap::new();
    info.insert(
        Bytes::from_static(b"files"),
        BencodeValue::List(vec![BencodeValue::Dict(f1)]),
    );
    info.insert(
        Bytes::from_static(b"name"),
        BencodeValue::Bytes(Bytes::from_static(b"bad\xffname")),
    );
    info.insert(
        Bytes::from_static(b"name.utf-8"),
        BencodeValue::string("好名字"),
    );
    info.insert(
        Bytes::from_static(b"piece length"),
        BencodeValue::Integer(-1),
    );

    let encoded = encode(&BencodeValue::Dict(info));
    let (name, total_size, files, piece_len) = parse_metadata(&encoded).unwrap();
    assert_eq!(name, "好名字");
    assert_eq!(total_size, 100);
    assert_eq!(files[0].path, "dir/你好.mp4");
    assert_eq!(piece_len, 0);

    let mut lossy_info = BTreeMap::new();
    lossy_info.insert(Bytes::from_static(b"length"), BencodeValue::Integer(50));
    lossy_info.insert(
        Bytes::from_static(b"name"),
        BencodeValue::Bytes(Bytes::from_static(b"bad\xff")),
    );
    let (lossy_name, size, _, _) = parse_metadata(&encode(&BencodeValue::Dict(lossy_info))).unwrap();
    assert_eq!(lossy_name, "bad\u{fffd}");
    assert_eq!(size, 50);

    let mut neg_info = BTreeMap::new();
    neg_info.insert(Bytes::from_static(b"length"), BencodeValue::Integer(-5));
    neg_info.insert(Bytes::from_static(b"name"), BencodeValue::string("neg"));
    assert!(parse_metadata(&encode(&BencodeValue::Dict(neg_info))).is_none());
}

#[test]
fn resolve_timeouts_clamps_connect_timeout_to_total_timeout() {
    assert_eq!(
        resolve_timeouts(6, 0),
        (Duration::from_secs(6), Duration::from_secs(2))
    );
    assert_eq!(
        resolve_timeouts(6, 1_500),
        (Duration::from_secs(6), Duration::from_millis(1_500))
    );
    assert_eq!(
        resolve_timeouts(1, 5_000),
        (Duration::from_secs(1), Duration::from_secs(1))
    );
    assert_eq!(
        resolve_timeouts(0, 0),
        (Duration::from_secs(15), Duration::from_secs(2))
    );
}

#[tokio::test]
async fn connect_timeout_records_tcp_stage_and_caches_endpoint_failure() {
    let stats = DhtRuntimeStats::default();
    let fetcher = RbitFetcher::new_with_options(6, 10, 32, 60, stats.clone());
    let addr: SocketAddr = "127.0.0.1:41999".parse().unwrap();
    let hash = [9u8; 20];

    fetcher.record_timeout_outcome(&hash, addr, MetadataTimeoutStage::TcpConnect);

    let runtime = stats.snapshot();
    assert_eq!(runtime.metadata_peer_failed, 1);
    assert_eq!(runtime.metadata_peer_timeouts, 1);
    let obs = stats.observability_snapshot();
    assert_eq!(obs.metadata_failure_timeout, 1);
    assert_eq!(obs.metadata_failure_connect, 0);
    assert_eq!(obs.metadata_timeout_stages.tcp_connect, 1);

    assert!(matches!(
        fetcher.fetch(&hash, addr).await,
        MetadataFetchOutcome::SkippedCached
    ));
    let after = stats.snapshot();
    assert_eq!(after.metadata_peer_failure_cache_hits, 1);
    let obs_after = stats.observability_snapshot();
    assert_eq!(obs_after.peer_cache_endpoint_hits, 1);
    assert_eq!(obs_after.peer_cache_timeout_hits, 1);
}
