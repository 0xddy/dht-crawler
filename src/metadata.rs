use crate::runtime_stats::{DhtRuntimeStats, MetadataConnectionError, MetadataTimeoutStage};
use crate::types::FileInfo;
use ahash::AHashMap;
use bytes::Bytes;
#[cfg(feature = "metrics")]
use metrics::{counter, gauge, histogram};
use rbit::peer::{ExtensionMessage, PeerTransport};
use rbit::{
    ExtensionHandshake, Handshake, Message, MetadataMessage, MetadataMessageType, PeerError,
    PeerId, metadata_piece_count,
};
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::time::timeout;

pub(crate) type FetchedMetadata = (String, u64, Vec<FileInfo>, u64);

pub(crate) enum MetadataFetchOutcome {
    Fetched(FetchedMetadata),
    Failed,
    SkippedCached,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetadataFetchFailure {
    Connect,
    NoExtension,
    Send,
    SizeLimit,
    Sha1,
    Parse,
    Reject,
    Other,
}

fn connection_error_category(error: &PeerError) -> MetadataConnectionError {
    use std::io::ErrorKind;
    match error {
        PeerError::Io(error) => match error.kind() {
            ErrorKind::ConnectionRefused => MetadataConnectionError::Refused,
            ErrorKind::ConnectionReset => MetadataConnectionError::Reset,
            ErrorKind::ConnectionAborted => MetadataConnectionError::Aborted,
            ErrorKind::UnexpectedEof => MetadataConnectionError::Closed,
            ErrorKind::TimedOut => MetadataConnectionError::Timeout,
            _ => MetadataConnectionError::IoOther,
        },
        PeerError::ConnectionClosed => MetadataConnectionError::Closed,
        PeerError::Timeout => MetadataConnectionError::Timeout,
        PeerError::InvalidHandshake => MetadataConnectionError::InvalidHandshake,
        PeerError::InfoHashMismatch => MetadataConnectionError::InfoHashMismatch,
        _ => MetadataConnectionError::Other,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerFailureReason {
    Timeout,
    ConnectFailed,
    MetadataFailed,
}

#[cfg(feature = "metrics")]
impl PeerFailureReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::ConnectFailed => "connect_failed",
            Self::MetadataFailed => "metadata_failed",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PeerFailureEntry {
    expires_at: Instant,
    reason: PeerFailureReason,
}

/// A reachable peer can serve other torrents even when one handshake or download fails.
/// Both scopes share a single configured capacity and TTL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum PeerFailureKey {
    Endpoint(SocketAddr),
    Metadata([u8; 20], SocketAddr),
}

#[derive(Default)]
struct PeerFailureCacheInner {
    entries: AHashMap<PeerFailureKey, PeerFailureEntry>,
    expiry: VecDeque<(Instant, PeerFailureKey)>,
}

struct PeerFailureCache {
    inner: Mutex<PeerFailureCacheInner>,
    capacity: usize,
    ttl: Duration,
}

impl PeerFailureCache {
    fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(PeerFailureCacheInner {
                entries: AHashMap::with_capacity(capacity.min(16_384)),
                expiry: VecDeque::with_capacity(capacity.min(16_384)),
            }),
            capacity,
            ttl,
        }
    }

    fn get(&self, key: PeerFailureKey, now: Instant) -> (Option<PeerFailureReason>, usize) {
        if self.capacity == 0 || self.ttl.is_zero() {
            return (None, 0);
        }
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::expire(&mut inner, now);
        (
            inner.entries.get(&key).map(|entry| entry.reason),
            inner.entries.len(),
        )
    }

    fn insert(&self, addr: PeerFailureKey, reason: PeerFailureReason, now: Instant) -> usize {
        if self.capacity == 0 || self.ttl.is_zero() {
            return 0;
        }
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::expire(&mut inner, now);
        while inner.entries.len() >= self.capacity && !inner.entries.contains_key(&addr) {
            let Some((expires_at, oldest_addr)) = inner.expiry.pop_front() else {
                break;
            };
            if inner
                .entries
                .get(&oldest_addr)
                .is_some_and(|entry| entry.expires_at == expires_at)
            {
                inner.entries.remove(&oldest_addr);
            }
        }

        let expires_at = now + self.ttl;
        inner
            .entries
            .insert(addr, PeerFailureEntry { expires_at, reason });
        inner.expiry.push_back((expires_at, addr));
        inner.entries.len()
    }

    fn remove(&self, addr: &PeerFailureKey, now: Instant) -> usize {
        if self.capacity == 0 || self.ttl.is_zero() {
            return 0;
        }
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::expire(&mut inner, now);
        inner.entries.remove(addr);
        inner.entries.len()
    }

    fn expire(inner: &mut PeerFailureCacheInner, now: Instant) {
        while let Some((expires_at, addr)) = inner.expiry.front().copied() {
            if expires_at > now {
                break;
            }
            inner.expiry.pop_front();
            if inner
                .entries
                .get(&addr)
                .is_some_and(|entry| entry.expires_at == expires_at)
            {
                inner.entries.remove(&addr);
            }
        }
    }
}

#[derive(Clone)]
/// BEP-9 Metadata fetcher with an end-to-end timeout and shared Peer failure cache.
pub struct RbitFetcher {
    total_timeout: Duration,
    runtime_stats: DhtRuntimeStats,
    peer_failure_cache: Arc<PeerFailureCache>,
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
        Self {
            total_timeout: Duration::from_secs(if timeout_secs == 0 { 15 } else { timeout_secs }),
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
                    MetadataFetchFailure::Other => self.runtime_stats.metadata_failure_other(),
                }
                #[cfg(feature = "metrics")]
                counter!("dht_metadata_fetch_result_total", "result" => "failed").increment(1);
                MetadataFetchOutcome::Failed
            }
            Err(_) => {
                let stage = MetadataTimeoutStage::from_index(stage.load(Ordering::Relaxed));
                self.record_timeout_failure(info_hash, peer_addr, stage);
                self.runtime_stats.metadata_peer_failed();
                self.runtime_stats.metadata_peer_timeout();
                self.runtime_stats.metadata_failure_timeout();
                self.runtime_stats.metadata_timeout_stage(stage);
                #[cfg(feature = "metrics")]
                {
                    counter!("dht_metadata_fetch_fail_total", "reason" => "timeout").increment(1);
                    counter!("dht_metadata_fetch_result_total", "result" => "timeout").increment(1);
                    counter!("dht_metadata_timeout_stage_total", "stage" => stage.as_str())
                        .increment(1);
                }
                MetadataFetchOutcome::Failed
            }
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
        let stream = TcpStream::connect(peer_addr).await.map_err(|error| {
            self.record_connection_failure(
                info_hash,
                peer_addr,
                MetadataTimeoutStage::TcpConnect,
                &PeerError::Io(error),
            );
            MetadataFetchFailure::Connect
        })?;
        // This is the same public transport, handshake encoder and buffered reader used by
        // rbit::PeerConnection::connect; retain unread extension frames from the handshake read.
        let mut transport = PeerTransport::new(stream);
        stage.store(MetadataTimeoutStage::BaseHandshake as u8, Ordering::Relaxed);
        let handshake_result = async {
            let peer_id = PeerId::generate();
            transport
                .send_handshake(&Handshake::new(*info_hash, *peer_id.as_bytes()))
                .await?;
            let remote = transport.receive_handshake().await?;
            if remote.info_hash != *info_hash {
                return Err(PeerError::InfoHashMismatch);
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
        error: &PeerError,
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

        let my_ut_metadata_id = 1;
        stage.store(
            MetadataTimeoutStage::ExtensionHandshakeSend as u8,
            Ordering::Relaxed,
        );
        let handshake = ExtensionHandshake::with_extensions(&[("ut_metadata", my_ut_metadata_id)]);
        let handshake_bytes = handshake.encode().map_err(|_| MetadataFetchFailure::Send)?;
        if conn
            .send_message(&Message::Extended {
                id: 0,
                payload: handshake_bytes,
            })
            .await
            .is_err()
        {
            #[cfg(feature = "metrics")]
            counter!("dht_metadata_fetch_fail_total", "reason" => "send_error").increment(1);
            return Err(MetadataFetchFailure::Send);
        }

        let mut metadata_size = 0;
        let mut remote_ut_metadata_id = 0;
        let mut pieces: BTreeMap<u32, Bytes> = BTreeMap::new();
        let mut total_received = 0usize;
        let mut request_sent = false;

        let info_bytes = loop {
            stage.store(
                if request_sent {
                    MetadataTimeoutStage::PieceReceive
                } else {
                    MetadataTimeoutStage::ExtensionHandshakeReceive
                } as u8,
                Ordering::Relaxed,
            );
            let msg = conn
                .receive_message()
                .await
                .map_err(|_| MetadataFetchFailure::Other)?;
            let Message::Extended { id, payload } = msg else {
                continue;
            };

            if id == 0 {
                if let Ok(ExtensionMessage::Handshake(remote_hs)) =
                    ExtensionMessage::decode(id, &payload)
                {
                    if let Some(size) = remote_hs.metadata_size {
                        metadata_size = size as u32;
                    }
                    if let Some(ext_id) = remote_hs.get_extension_id("ut_metadata") {
                        remote_ut_metadata_id = ext_id;
                    }
                }

                if metadata_size > 0 && remote_ut_metadata_id > 0 && !request_sent {
                    if metadata_size > 10 * 1024 * 1024 {
                        #[cfg(feature = "metrics")]
                        counter!("dht_metadata_fetch_fail_total", "reason" => "size_limit")
                            .increment(1);
                        return Err(MetadataFetchFailure::SizeLimit);
                    }

                    let count = metadata_piece_count(metadata_size as usize);
                    stage.store(
                        MetadataTimeoutStage::PieceRequestSend as u8,
                        Ordering::Relaxed,
                    );
                    for piece in 0..count {
                        let encoded = MetadataMessage::request(piece as u32)
                            .encode()
                            .map_err(|_| MetadataFetchFailure::Send)?;
                        if conn
                            .send_message(&Message::Extended {
                                id: remote_ut_metadata_id,
                                payload: encoded,
                            })
                            .await
                            .is_err()
                        {
                            #[cfg(feature = "metrics")]
                            counter!("dht_metadata_fetch_fail_total", "reason" => "send_error")
                                .increment(1);
                            return Err(MetadataFetchFailure::Send);
                        }
                    }
                    request_sent = true;
                }
                continue;
            }

            if id != my_ut_metadata_id {
                continue;
            }
            let Ok(meta_msg) = MetadataMessage::decode(&payload) else {
                continue;
            };
            if request_sent
                && meta_msg.msg_type == MetadataMessageType::Reject
                && (meta_msg.piece as usize) < metadata_piece_count(metadata_size as usize)
            {
                #[cfg(feature = "metrics")]
                counter!("dht_metadata_fetch_fail_total", "reason" => "reject").increment(1);
                return Err(MetadataFetchFailure::Reject);
            }
            if meta_msg.msg_type != MetadataMessageType::Data {
                continue;
            }
            let Some(data) = meta_msg.data else {
                continue;
            };

            #[cfg(feature = "metrics")]
            counter!("dht_metadata_bytes_downloaded_total").increment(data.len() as u64);
            self.runtime_stats.metadata_bytes_downloaded(data.len());

            let data_len = data.len();
            if let Some(previous) = pieces.insert(meta_msg.piece, data) {
                total_received = total_received.saturating_sub(previous.len());
            }
            total_received = total_received.saturating_add(data_len);

            if metadata_size == 0 || total_received < metadata_size as usize {
                continue;
            }

            let count = metadata_piece_count(metadata_size as usize);
            let mut full_data = Vec::with_capacity(metadata_size as usize);
            for piece in 0..count {
                let data = pieces
                    .get(&(piece as u32))
                    .ok_or(MetadataFetchFailure::Other)?;
                full_data.extend_from_slice(data);
            }

            let info_hash_copy = *info_hash;
            stage.store(MetadataTimeoutStage::Validation as u8, Ordering::Relaxed);
            let validated = tokio::task::spawn_blocking(move || {
                let mut hasher = Sha1::new();
                hasher.update(&full_data);
                let digest: [u8; 20] = hasher.finalize().into();
                (digest == info_hash_copy).then_some(full_data)
            })
            .await
            .ok()
            .flatten();

            match validated {
                Some(data) => {
                    #[cfg(feature = "metrics")]
                    counter!("dht_metadata_handshake_result_total", "result" => "success")
                        .increment(1);
                    break data;
                }
                None => {
                    #[cfg(feature = "metrics")]
                    counter!("dht_metadata_fetch_fail_total", "reason" => "sha1_mismatch")
                        .increment(1);
                    return Err(MetadataFetchFailure::Sha1);
                }
            }
        };

        self.runtime_stats.observe_metadata_size(info_bytes.len());
        match parse_metadata(&info_bytes) {
            Some(metadata) => {
                #[cfg(feature = "metrics")]
                histogram!("dht_metadata_size_bytes").record(info_bytes.len() as f64);
                Ok(metadata)
            }
            None => {
                #[cfg(feature = "metrics")]
                counter!("dht_metadata_fetch_fail_total", "reason" => "parse_error").increment(1);
                Err(MetadataFetchFailure::Parse)
            }
        }
    }
}

fn parse_metadata(info_bytes: &[u8]) -> Option<FetchedMetadata> {
    let value = rbit::decode(info_bytes).ok()?;
    let dict = value.as_dict()?;
    let name = dict
        .get(&b"name"[..])
        .and_then(|value| value.as_str())
        .unwrap_or("Unknown")
        .to_string();
    let piece_length = dict
        .get(&b"piece length"[..])
        .and_then(|value| value.as_integer())
        .unwrap_or(0) as u64;

    let mut total_size = 0;
    let mut file_list = Vec::new();
    if let Some(files) = dict.get(&b"files"[..]).and_then(|value| value.as_list()) {
        for file in files {
            let Some(file_dict) = file.as_dict() else {
                continue;
            };
            let Some(length) = file_dict
                .get(&b"length"[..])
                .and_then(|value| value.as_integer())
            else {
                continue;
            };
            let length = length as u64;
            total_size += length;
            let path = file_dict
                .get(&b"path"[..])
                .and_then(|value| value.as_list())
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part.as_str())
                        .collect::<Vec<_>>()
                        .join("/")
                })
                .unwrap_or_default();
            file_list.push(FileInfo { path, size: length });
        }
    } else if let Some(length) = dict
        .get(&b"length"[..])
        .and_then(|value| value.as_integer())
    {
        total_size = length as u64;
        file_list.push(FileInfo {
            path: name.clone(),
            size: total_size,
        });
    }

    (total_size > 0).then_some((name, total_size, file_list, piece_length))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MetadataConnectionErrorsSnapshot, MetadataTimeoutStagesSnapshot};
    use rbit::PeerConnection;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn serve_metadata(mut conn: PeerConnection, metadata: Bytes, reject: bool) {
        assert!(matches!(
            conn.receive().await.unwrap(),
            Message::Extended { id: 0, .. }
        ));
        let mut handshake = ExtensionHandshake::with_extensions(&[("ut_metadata", 3)]);
        handshake.metadata_size = Some(metadata.len() as i64);
        conn.send(Message::Extended {
            id: 0,
            payload: handshake.encode().unwrap(),
        })
        .await
        .unwrap();
        let Message::Extended { id: 3, payload } = conn.receive().await.unwrap() else {
            panic!("expected metadata request using the remote extension ID");
        };
        assert_eq!(MetadataMessage::decode(&payload).unwrap().piece, 0);
        let message = if reject {
            MetadataMessage::reject(0)
        } else {
            MetadataMessage::data(0, metadata.len() as u32, metadata)
        };
        conn.send(Message::Extended {
            id: 1,
            payload: message.encode().unwrap(),
        })
        .await
        .unwrap();
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
                            let mut conn = PeerConnection::accept(stream, [7; 20], [9; 20])
                                .await
                                .unwrap();
                            if failure == "reject" {
                                serve_metadata(conn, bad_metadata, true).await;
                            } else {
                                conn.receive().await.unwrap();
                                let mut hs =
                                    ExtensionHandshake::with_extensions(&[("ut_metadata", 3)]);
                                hs.metadata_size = Some(1);
                                conn.send(Message::Extended {
                                    id: 0,
                                    payload: hs.encode().unwrap(),
                                })
                                .await
                                .unwrap();
                                conn.receive().await.unwrap();
                                std::future::pending::<()>().await;
                            }
                        }
                    }
                });
                let (stream, _) = listener.accept().await.unwrap();
                let conn = PeerConnection::accept(stream, good_hash, [9; 20])
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
                    let conn = PeerConnection::accept(stream, hash, [9; 20]).await.unwrap();
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
                let mut conn = PeerConnection::accept(stream, [7; 20], [9; 20])
                    .await
                    .unwrap();
                assert!(matches!(
                    conn.receive().await.unwrap(),
                    Message::Extended { id: 0, .. }
                ));
                if request_pieces {
                    let mut handshake = ExtensionHandshake::with_extensions(&[("ut_metadata", 3)]);
                    handshake.metadata_size = Some(1);
                    conn.send(Message::Extended {
                        id: 0,
                        payload: handshake.encode().unwrap(),
                    })
                    .await
                    .unwrap();
                    let Message::Extended { id: 3, payload } = conn.receive().await.unwrap() else {
                        panic!("expected metadata request");
                    };
                    assert_eq!(
                        MetadataMessage::decode(&payload).unwrap().msg_type,
                        MetadataMessageType::Request
                    );
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
            rbit::Handshake::new([8; 20], [9; 20]).encode().to_vec(),
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
        use std::io::{Error, ErrorKind};
        for (kind, expected) in [
            (
                ErrorKind::ConnectionRefused,
                MetadataConnectionError::Refused,
            ),
            (ErrorKind::ConnectionReset, MetadataConnectionError::Reset),
            (
                ErrorKind::ConnectionAborted,
                MetadataConnectionError::Aborted,
            ),
            (ErrorKind::UnexpectedEof, MetadataConnectionError::Closed),
            (ErrorKind::TimedOut, MetadataConnectionError::Timeout),
            (ErrorKind::Other, MetadataConnectionError::IoOther),
        ] {
            assert_eq!(
                connection_error_category(&PeerError::Io(Error::new(kind, "opaque"))),
                expected
            );
        }
        assert_eq!(
            connection_error_category(&PeerError::Timeout),
            MetadataConnectionError::Timeout
        );
        assert_eq!(
            connection_error_category(&PeerError::Protocol("connection refused".into())),
            MetadataConnectionError::Other
        );
        assert_eq!(
            connection_error_category(&PeerError::Io(Error::other("timeout"))),
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
            let mut conn = PeerConnection::accept(stream, info_hash, [9; 20])
                .await
                .unwrap();
            assert!(matches!(
                conn.receive().await.unwrap(),
                Message::Extended { id: 0, .. }
            ));
            let mut handshake = ExtensionHandshake::with_extensions(&[("ut_metadata", 3)]);
            handshake.metadata_size = Some(metadata.len() as i64);
            conn.send(Message::Extended {
                id: 0,
                payload: handshake.encode().unwrap(),
            })
            .await
            .unwrap();
            assert!(matches!(
                conn.receive().await.unwrap(),
                Message::Extended { id: 3, .. }
            ));
            conn.send(Message::Extended {
                id: 1,
                payload: MetadataMessage::data(0, metadata.len() as u32, metadata)
                    .encode()
                    .unwrap(),
            })
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
}
