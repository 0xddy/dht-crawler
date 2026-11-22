pub(crate) mod bencode;
mod failure_cache;
mod parse;
#[cfg(test)]
mod tests;
pub(crate) mod wire;

use crate::runtime_stats::{DhtRuntimeStats, MetadataConnectionError, MetadataTimeoutStage};
use crate::types::FileInfo;
use bytes::Bytes;
use failure_cache::{PeerFailureCache, PeerFailureKey, PeerFailureReason};
#[cfg(feature = "metrics")]
use metrics::{counter, gauge, histogram};
use parse::parse_metadata;
use sha1::{Digest, Sha1};
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::time::timeout;
use wire::{
    Handshake, MetadataMsg, OUR_UT_METADATA_ID, PeerTransport, PeerWireError,
    RemoteExtensionHandshake, random_peer_id,
};

const MAX_METADATA_SIZE: i64 = 10 * 1024 * 1024;
const METADATA_PIECE_SIZE: usize = 16 * 1024;
const MAX_IN_FLIGHT_PIECE_REQUESTS: usize = 16;
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_millis(2_000);

pub(crate) type FetchedMetadata = (String, u64, Vec<FileInfo>, u64);

pub(crate) fn metadata_piece_count(size: usize) -> usize {
    size.div_ceil(METADATA_PIECE_SIZE)
}

pub(crate) enum MetadataFetchOutcome {
    Fetched(FetchedMetadata),
    Failed,
    SkippedCached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetadataFetchFailure {
    Connect,
    ConnectTimeout,
    NoExtension,
    Send,
    SizeLimit,
    Sha1,
    Parse,
    Reject,
    PeerClosed,
    Protocol,
    Other,
}

fn connection_error_category(error: &PeerWireError) -> MetadataConnectionError {
    match error {
        PeerWireError::Io(error) => match error.kind() {
            ErrorKind::ConnectionRefused => MetadataConnectionError::Refused,
            ErrorKind::ConnectionReset => MetadataConnectionError::Reset,
            ErrorKind::ConnectionAborted => MetadataConnectionError::Aborted,
            ErrorKind::UnexpectedEof => MetadataConnectionError::Closed,
            ErrorKind::TimedOut => MetadataConnectionError::Timeout,
            _ => MetadataConnectionError::IoOther,
        },
        PeerWireError::InvalidHandshake => MetadataConnectionError::InvalidHandshake,
        PeerWireError::InfoHashMismatch => MetadataConnectionError::InfoHashMismatch,
        PeerWireError::MessageTooLarge(_) | PeerWireError::InvalidFrame => {
            MetadataConnectionError::Other
        }
    }
}

fn map_wire_receive_error(error: PeerWireError) -> MetadataFetchFailure {
    match error {
        PeerWireError::Io(ref io_err)
            if matches!(
                io_err.kind(),
                ErrorKind::UnexpectedEof
                    | ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe
            ) =>
        {
            MetadataFetchFailure::PeerClosed
        }
        PeerWireError::MessageTooLarge(_)
        | PeerWireError::InvalidFrame
        | PeerWireError::InvalidHandshake
        | PeerWireError::InfoHashMismatch => MetadataFetchFailure::Protocol,
        PeerWireError::Io(_) => MetadataFetchFailure::Other,
    }
}

#[derive(Clone)]
/// BEP-9 Metadata fetcher with an end-to-end timeout and shared Peer failure cache.
pub struct RbitFetcher {
    total_timeout: Duration,
    connect_timeout: Duration,
    runtime_stats: DhtRuntimeStats,
    peer_failure_cache: Arc<PeerFailureCache>,
}

fn resolve_timeouts(timeout_secs: u64, connect_timeout_ms: u64) -> (Duration, Duration) {
    let total = Duration::from_secs(if timeout_secs == 0 { 15 } else { timeout_secs });
    let connect = if connect_timeout_ms == 0 {
        DEFAULT_CONNECT_TIMEOUT
    } else {
        Duration::from_millis(connect_timeout_ms)
    }
    .min(total);
    (total, connect)
}

impl RbitFetcher {
    /// Creates a standalone fetcher with the default failure-cache capacity and TTL.
    ///
    /// [`DHTServer`](crate::DHTServer) normally constructs this component from
    /// [`MetadataOptions`](crate::MetadataOptions).
    pub fn new(timeout_secs: u64) -> Self {
        Self::new_with_runtime_stats(timeout_secs, 200_000, 60, DhtRuntimeStats::default())
    }

    pub(crate) fn new_with_runtime_stats(
        timeout_secs: u64,
        peer_failure_cache_capacity: usize,
        peer_failure_ttl_secs: u64,
        runtime_stats: DhtRuntimeStats,
    ) -> Self {
        Self::new_with_options(
            timeout_secs,
            0,
            peer_failure_cache_capacity,
            peer_failure_ttl_secs,
            runtime_stats,
        )
    }

    pub(crate) fn new_with_options(
        timeout_secs: u64,
        connect_timeout_ms: u64,
        peer_failure_cache_capacity: usize,
        peer_failure_ttl_secs: u64,
        runtime_stats: DhtRuntimeStats,
    ) -> Self {
        let (total_timeout, connect_timeout) = resolve_timeouts(timeout_secs, connect_timeout_ms);
        Self {
            total_timeout,
            connect_timeout,
            runtime_stats,
            peer_failure_cache: Arc::new(PeerFailureCache::new(
                peer_failure_cache_capacity,
                Duration::from_secs(peer_failure_ttl_secs),
            )),
        }
    }

    /// Fetch metadata from one peer under a single end-to-end deadline.
    ///
    /// The deadline covers TCP connect, both BitTorrent handshakes, all metadata
    /// piece I/O and the wait for hash validation. Inner library timeouts cannot
    /// stack on top of this deadline. Like any Tokio timeout, it is cooperative:
    /// synchronous work such as bencode parsing cannot be preempted mid-poll.
    #[cfg(test)]
    pub(crate) async fn fetch(
        &self,
        info_hash: &[u8; 20],
        peer_addr: SocketAddr,
    ) -> MetadataFetchOutcome {
        self.fetch_with_attempt_observer(info_hash, peer_addr, || {})
            .await
    }

    pub(crate) async fn fetch_with_attempt_observer<F>(
        &self,
        info_hash: &[u8; 20],
        peer_addr: SocketAddr,
        on_attempt: F,
    ) -> MetadataFetchOutcome
    where
        F: FnOnce() + Send,
    {
        for key in [
            PeerFailureKey::Endpoint(peer_addr),
            PeerFailureKey::Metadata(*info_hash, peer_addr),
        ] {
            let (cached_reason, cache_entries) = self.peer_failure_cache.get(key, Instant::now());
            self.set_peer_failure_cache_entries(cache_entries);
            if let Some(reason) = cached_reason {
                self.runtime_stats.metadata_peer_failure_cache_hit();
                match key {
                    PeerFailureKey::Endpoint(_) => self.runtime_stats.peer_cache_endpoint_hits(),
                    PeerFailureKey::Metadata(_, _) => self.runtime_stats.peer_cache_metadata_hits(),
                }
                match reason {
                    PeerFailureReason::Timeout => self.runtime_stats.peer_cache_hit_timeout(),
                    PeerFailureReason::ConnectFailed => self.runtime_stats.peer_cache_hit_connect(),
                    PeerFailureReason::MetadataFailed => {}
                }
                #[cfg(feature = "metrics")]
                counter!("dht_metadata_peer_failure_cache_hits_total", "reason" => reason.as_str(), "scope" => match key { PeerFailureKey::Endpoint(_) => "endpoint", PeerFailureKey::Metadata(_, _) => "metadata" }).increment(1);
                return MetadataFetchOutcome::SkippedCached;
            }
        }

        on_attempt();
        self.runtime_stats.metadata_peer_attempt();
        #[cfg(feature = "metrics")]
        {
            counter!("dht_metadata_fetch_attempts_total").increment(1);
            counter!("dht_metadata_peer_attempts_total").increment(1);
        }

        let started = Instant::now();
        let stage = AtomicU8::new(MetadataTimeoutStage::TcpConnect as u8);
        let result = timeout(
            self.total_timeout,
            self.fetch_with_peer(info_hash, peer_addr, &stage),
        )
        .await;

        #[cfg(feature = "metrics")]
        histogram!("dht_metadata_fetch_duration_seconds").record(started.elapsed().as_secs_f64());
        self.runtime_stats.observe_metadata_fetch_duration(
            started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        );

        match result {
            Ok(Ok(metadata)) => {
                self.peer_failure_cache
                    .remove(&PeerFailureKey::Endpoint(peer_addr), Instant::now());
                let cache_entries = self.peer_failure_cache.remove(
                    &PeerFailureKey::Metadata(*info_hash, peer_addr),
                    Instant::now(),
                );
                self.set_peer_failure_cache_entries(cache_entries);
                self.runtime_stats.metadata_peer_succeeded();
                #[cfg(feature = "metrics")]
                {
                    counter!("dht_metadata_fetch_success_total").increment(1);
                    counter!("dht_metadata_fetch_result_total", "result" => "success").increment(1);
                }
                MetadataFetchOutcome::Fetched(metadata)
            }
            Ok(Err(MetadataFetchFailure::ConnectTimeout)) => {
                self.record_timeout_outcome(info_hash, peer_addr, MetadataTimeoutStage::TcpConnect);
                MetadataFetchOutcome::Failed
            }
            Ok(Err(reason)) => {
                // TCP/base-handshake errors are classified at their boundary. All remaining
                // remote metadata failures belong to this torrent, not the whole endpoint.
                if reason != MetadataFetchFailure::Connect {
                    self.record_peer_failure(
                        PeerFailureKey::Metadata(*info_hash, peer_addr),
                        PeerFailureReason::MetadataFailed,
                    );
                }
                self.runtime_stats.metadata_peer_failed();
                match reason {
                    MetadataFetchFailure::Connect => self.runtime_stats.metadata_failure_connect(),
                    MetadataFetchFailure::ConnectTimeout => {
                        debug_assert!(false, "ConnectTimeout handled in dedicated branch");
                    }
                    MetadataFetchFailure::NoExtension => {
                        self.runtime_stats.metadata_failure_no_extension()
                    }
                    MetadataFetchFailure::Send => self.runtime_stats.metadata_failure_send(),
                    MetadataFetchFailure::SizeLimit => {
                        self.runtime_stats.metadata_failure_size_limit()
                    }
                    MetadataFetchFailure::Sha1 => self.runtime_stats.metadata_failure_sha1(),
                    MetadataFetchFailure::Parse => self.runtime_stats.metadata_failure_parse(),
                    MetadataFetchFailure::Reject => self.runtime_stats.metadata_failure_reject(),
                    MetadataFetchFailure::PeerClosed => {
                        self.runtime_stats.metadata_failure_peer_closed();
                        #[cfg(feature = "metrics")]
                        counter!("dht_metadata_fetch_fail_total", "reason" => "peer_closed")
                            .increment(1);
                    }
                    MetadataFetchFailure::Protocol => {
                        self.runtime_stats.metadata_failure_protocol();
                        #[cfg(feature = "metrics")]
                        counter!("dht_metadata_fetch_fail_total", "reason" => "protocol_error")
                            .increment(1);
                    }
                    MetadataFetchFailure::Other => self.runtime_stats.metadata_failure_other(),
                }
                #[cfg(feature = "metrics")]
                counter!("dht_metadata_fetch_result_total", "result" => "failed").increment(1);
                MetadataFetchOutcome::Failed
            }
            Err(_) => {
                let stage = MetadataTimeoutStage::from_index(stage.load(Ordering::Relaxed));
                self.record_timeout_outcome(info_hash, peer_addr, stage);
                MetadataFetchOutcome::Failed
            }
        }
    }

    fn record_timeout_outcome(
        &self,
        info_hash: &[u8; 20],
        peer_addr: SocketAddr,
        stage: MetadataTimeoutStage,
    ) {
        self.record_timeout_failure(info_hash, peer_addr, stage);
        self.runtime_stats.metadata_peer_failed();
        self.runtime_stats.metadata_peer_timeout();
        self.runtime_stats.metadata_failure_timeout();
        self.runtime_stats.metadata_timeout_stage(stage);
        #[cfg(feature = "metrics")]
        {
            counter!("dht_metadata_fetch_fail_total", "reason" => "timeout").increment(1);
            counter!("dht_metadata_fetch_result_total", "result" => "timeout").increment(1);
            counter!("dht_metadata_timeout_stage_total", "stage" => stage.as_str()).increment(1);
        }
    }

    fn record_timeout_failure(
        &self,
        info_hash: &[u8; 20],
        addr: SocketAddr,
        stage: MetadataTimeoutStage,
    ) {
        let key = match stage {
            MetadataTimeoutStage::TcpConnect => PeerFailureKey::Endpoint(addr),
            // Blocking-pool congestion and local validation are not a peer failure.
            MetadataTimeoutStage::Validation => return,
            _ => PeerFailureKey::Metadata(*info_hash, addr),
        };
        self.record_peer_failure(key, PeerFailureReason::Timeout);
    }

    fn record_peer_failure(&self, peer_addr: PeerFailureKey, reason: PeerFailureReason) {
        let cache_entries = self
            .peer_failure_cache
            .insert(peer_addr, reason, Instant::now());
        self.set_peer_failure_cache_entries(cache_entries);
        #[cfg(feature = "metrics")]
        counter!("dht_metadata_peer_failure_cache_inserts_total", "reason" => reason.as_str())
            .increment(1);
    }

    fn set_peer_failure_cache_entries(&self, count: usize) {
        self.runtime_stats
            .set_metadata_peer_failure_cache_entries(count);
        #[cfg(feature = "metrics")]
        gauge!("dht_metadata_peer_failure_cache_entries").set(count as f64);
    }

    async fn connect_peer(
        &self,
        info_hash: &[u8; 20],
        peer_addr: SocketAddr,
        stage: &AtomicU8,
    ) -> Result<PeerTransport, MetadataFetchFailure> {
        let stream = match timeout(self.connect_timeout, TcpStream::connect(peer_addr)).await {
            Ok(Ok(stream)) => stream,
            Ok(Err(error)) => {
                self.record_connection_failure(
                    info_hash,
                    peer_addr,
                    MetadataTimeoutStage::TcpConnect,
                    &PeerWireError::Io(error),
                );
                return Err(MetadataFetchFailure::Connect);
            }
            Err(_) => return Err(MetadataFetchFailure::ConnectTimeout),
        };
        let _ = stream.set_nodelay(true);
        let mut transport = PeerTransport::new(stream);
        stage.store(MetadataTimeoutStage::BaseHandshake as u8, Ordering::Relaxed);
        let handshake_result = async {
            let peer_id = random_peer_id();
            transport
                .send_handshake(&Handshake::new(*info_hash, peer_id))
                .await?;
            let remote = transport.receive_handshake().await?;
            if remote.info_hash != *info_hash {
                return Err(PeerWireError::InfoHashMismatch);
            }
            Ok(remote.supports_extension_protocol())
        }
        .await;
        let supports_extension = handshake_result.map_err(|error| {
            self.record_connection_failure(
                info_hash,
                peer_addr,
                MetadataTimeoutStage::BaseHandshake,
                &error,
            );
            MetadataFetchFailure::Connect
        })?;
        #[cfg(feature = "metrics")]
        counter!("dht_metadata_connection_result_total", "result" => "success").increment(1);
        if !supports_extension {
            self.runtime_stats.metadata_no_extension();
            #[cfg(feature = "metrics")]
            counter!("dht_metadata_handshake_result_total", "result" => "no_extension_support")
                .increment(1);
            return Err(MetadataFetchFailure::NoExtension);
        }
        Ok(transport)
    }

    fn record_connection_failure(
        &self,
        info_hash: &[u8; 20],
        addr: SocketAddr,
        stage: MetadataTimeoutStage,
        error: &PeerWireError,
    ) {
        let category = connection_error_category(error);
        self.runtime_stats.metadata_connection_error(category);
        self.runtime_stats.metadata_connect_failed();
        let key = if stage == MetadataTimeoutStage::TcpConnect {
            self.runtime_stats.metadata_failure_tcp_connect();
            PeerFailureKey::Endpoint(addr)
        } else {
            self.runtime_stats.metadata_failure_base_handshake();
            PeerFailureKey::Metadata(*info_hash, addr)
        };
        self.record_peer_failure(key, PeerFailureReason::ConnectFailed);
        #[cfg(feature = "metrics")]
        {
            counter!("dht_metadata_connection_result_total", "result" => "failed").increment(1);
            counter!("dht_metadata_connection_error_total", "reason" => category.as_str())
                .increment(1);
        }
    }

    async fn fetch_with_peer(
        &self,
        info_hash: &[u8; 20],
        peer_addr: SocketAddr,
        stage: &AtomicU8,
    ) -> Result<FetchedMetadata, MetadataFetchFailure> {
        let mut conn = self.connect_peer(info_hash, peer_addr, stage).await?;

        stage.store(
            MetadataTimeoutStage::ExtensionHandshakeSend as u8,
            Ordering::Relaxed,
        );
        if conn.send_our_extension_handshake().await.is_err() {
            #[cfg(feature = "metrics")]
            counter!("dht_metadata_fetch_fail_total", "reason" => "send_error").increment(1);
            return Err(MetadataFetchFailure::Send);
        }

        let mut metadata_size = 0usize;
        let mut total_pieces = 0usize;
        let mut next_piece_to_request = 0usize;
        let mut remote_ut_metadata_id = 0u8;
        let mut pieces: BTreeMap<u32, Bytes> = BTreeMap::new();
        let mut total_received = 0usize;
        let mut request_sent = false;

        loop {
            stage.store(
                if request_sent {
                    MetadataTimeoutStage::PieceReceive
                } else {
                    MetadataTimeoutStage::ExtensionHandshakeReceive
                } as u8,
                Ordering::Relaxed,
            );
            let (id, payload) = conn
                .receive_next_extended()
                .await
                .map_err(map_wire_receive_error)?;

            if id == 0 {
                let remote_hs = RemoteExtensionHandshake::decode(&payload)
                    .map_err(|_| MetadataFetchFailure::Protocol)?;

                if !request_sent {
                    let Some(ext_id) = remote_hs.ut_metadata_id else {
                        self.runtime_stats.metadata_no_extension();
                        #[cfg(feature = "metrics")]
                        counter!("dht_metadata_handshake_result_total", "result" => "no_extension_support")
                            .increment(1);
                        return Err(MetadataFetchFailure::NoExtension);
                    };
                    let Some(size) = remote_hs.metadata_size else {
                        #[cfg(feature = "metrics")]
                        counter!("dht_metadata_fetch_fail_total", "reason" => "reject").increment(1);
                        return Err(MetadataFetchFailure::Reject);
                    };
                    if size <= 0 {
                        #[cfg(feature = "metrics")]
                        counter!("dht_metadata_fetch_fail_total", "reason" => "reject").increment(1);
                        return Err(MetadataFetchFailure::Reject);
                    }
                    if size > MAX_METADATA_SIZE {
                        #[cfg(feature = "metrics")]
                        counter!("dht_metadata_fetch_fail_total", "reason" => "size_limit")
                            .increment(1);
                        return Err(MetadataFetchFailure::SizeLimit);
                    }

                    remote_ut_metadata_id = ext_id;
                    metadata_size = size as usize;
                    total_pieces = metadata_piece_count(metadata_size);
                    let initial_window = total_pieces.min(MAX_IN_FLIGHT_PIECE_REQUESTS);
                    stage.store(
                        MetadataTimeoutStage::PieceRequestSend as u8,
                        Ordering::Relaxed,
                    );
                    for piece in 0..initial_window {
                        send_piece_request(&mut conn, remote_ut_metadata_id, piece as u32).await?;
                    }
                    next_piece_to_request = initial_window;
                    request_sent = true;
                }
                continue;
            }

            if id != OUR_UT_METADATA_ID || !request_sent {
                continue;
            }
            let Ok(meta_msg) = MetadataMsg::decode(payload) else {
                continue;
            };
            match meta_msg {
                MetadataMsg::Reject { piece } => {
                    if (piece as usize) < total_pieces {
                        #[cfg(feature = "metrics")]
                        counter!("dht_metadata_fetch_fail_total", "reason" => "reject")
                            .increment(1);
                        return Err(MetadataFetchFailure::Reject);
                    }
                }
                MetadataMsg::Request { .. } => {}
                MetadataMsg::Data { piece, data } => {
                    let piece_idx = piece as usize;
                    if piece_idx >= total_pieces
                        || data.len() != expected_piece_len(piece_idx, total_pieces, metadata_size)
                    {
                        return Err(MetadataFetchFailure::Protocol);
                    }

                    #[cfg(feature = "metrics")]
                    counter!("dht_metadata_bytes_downloaded_total").increment(data.len() as u64);
                    self.runtime_stats.metadata_bytes_downloaded(data.len());

                    let data_len = data.len();
                    if let Some(previous) = pieces.insert(piece, data) {
                        total_received = total_received.saturating_sub(previous.len());
                    } else if next_piece_to_request < total_pieces {
                        stage.store(
                            MetadataTimeoutStage::PieceRequestSend as u8,
                            Ordering::Relaxed,
                        );
                        send_piece_request(
                            &mut conn,
                            remote_ut_metadata_id,
                            next_piece_to_request as u32,
                        )
                        .await?;
                        next_piece_to_request += 1;
                    }
                    total_received = total_received.saturating_add(data_len);

                    if pieces.len() < total_pieces || total_received < metadata_size {
                        continue;
                    }

                    let mut full_data = Vec::with_capacity(metadata_size);
                    for idx in 0..total_pieces {
                        let chunk = pieces
                            .get(&(idx as u32))
                            .ok_or(MetadataFetchFailure::Protocol)?;
                        full_data.extend_from_slice(chunk);
                    }

                    let info_hash_copy = *info_hash;
                    stage.store(MetadataTimeoutStage::Validation as u8, Ordering::Relaxed);
                    let validation_result = tokio::task::spawn_blocking(move || {
                        let mut hasher = Sha1::new();
                        hasher.update(&full_data);
                        let digest: [u8; 20] = hasher.finalize().into();
                        if digest != info_hash_copy {
                            return Err(MetadataFetchFailure::Sha1);
                        }
                        let len = full_data.len();
                        parse_metadata(&full_data)
                            .map(|metadata| (len, metadata))
                            .ok_or(MetadataFetchFailure::Parse)
                    })
                    .await
                    .map_err(|_| MetadataFetchFailure::Other)?;

                    match validation_result {
                        Ok((len, metadata)) => {
                            #[cfg(feature = "metrics")]
                            {
                                counter!(
                                    "dht_metadata_handshake_result_total",
                                    "result" => "success"
                                )
                                .increment(1);
                                histogram!("dht_metadata_size_bytes").record(len as f64);
                            }
                            self.runtime_stats.observe_metadata_size(len);
                            return Ok(metadata);
                        }
                        Err(reason @ MetadataFetchFailure::Sha1) => {
                            #[cfg(feature = "metrics")]
                            counter!("dht_metadata_fetch_fail_total", "reason" => "sha1_mismatch")
                                .increment(1);
                            return Err(reason);
                        }
                        Err(reason @ MetadataFetchFailure::Parse) => {
                            #[cfg(feature = "metrics")]
                            counter!("dht_metadata_fetch_fail_total", "reason" => "parse_error")
                                .increment(1);
                            return Err(reason);
                        }
                        Err(reason) => return Err(reason),
                    }
                }
            }
        }
    }
}

async fn send_piece_request(
    conn: &mut PeerTransport,
    remote_ut_metadata_id: u8,
    piece: u32,
) -> Result<(), MetadataFetchFailure> {
    let encoded = MetadataMsg::encode_request(piece);
    if conn
        .send_extended(remote_ut_metadata_id, &encoded)
        .await
        .is_err()
    {
        #[cfg(feature = "metrics")]
        counter!("dht_metadata_fetch_fail_total", "reason" => "send_error").increment(1);
        return Err(MetadataFetchFailure::Send);
    }
    Ok(())
}

fn expected_piece_len(piece: usize, total_pieces: usize, metadata_size: usize) -> usize {
    if piece + 1 < total_pieces {
        METADATA_PIECE_SIZE
    } else {
        metadata_size.saturating_sub(piece * METADATA_PIECE_SIZE)
    }
}
