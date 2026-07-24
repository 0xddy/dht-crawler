use crate::runtime_stats::DhtRuntimeStats;
use crate::types::FileInfo;
use ahash::AHashMap;
use bytes::Bytes;
#[cfg(feature = "metrics")]
use metrics::{counter, gauge, histogram};
use rbit::peer::ExtensionMessage;
use rbit::{
    ExtensionHandshake, Message, MetadataMessage, MetadataMessageType, PeerConnection, PeerId,
    metadata_piece_count,
};
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
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
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PeerFailureReason {
    Timeout,
    ConnectFailed,
}

#[cfg(feature = "metrics")]
impl PeerFailureReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::ConnectFailed => "connect_failed",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PeerFailureEntry {
    expires_at: Instant,
    reason: PeerFailureReason,
}

#[derive(Default)]
struct PeerFailureCacheInner {
    entries: AHashMap<SocketAddr, PeerFailureEntry>,
    expiry: VecDeque<(Instant, SocketAddr)>,
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

    fn get(&self, addr: SocketAddr, now: Instant) -> (Option<PeerFailureReason>, usize) {
        if self.capacity == 0 || self.ttl.is_zero() {
            return (None, 0);
        }
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::expire(&mut inner, now);
        (
            inner.entries.get(&addr).map(|entry| entry.reason),
            inner.entries.len(),
        )
    }

    fn insert(&self, addr: SocketAddr, reason: PeerFailureReason, now: Instant) -> usize {
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

    fn remove(&self, addr: &SocketAddr, now: Instant) -> usize {
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
    /// piece I/O, hash validation and bencode parsing. Inner library timeouts can
    /// therefore never stack on top of the configured metadata timeout.
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
        let (cached_reason, cache_entries) = self.peer_failure_cache.get(peer_addr, Instant::now());
        self.set_peer_failure_cache_entries(cache_entries);
        if let Some(reason) = cached_reason {
            self.runtime_stats.metadata_peer_failure_cache_hit();
            match reason {
                PeerFailureReason::Timeout => self.runtime_stats.peer_cache_hit_timeout(),
                PeerFailureReason::ConnectFailed => self.runtime_stats.peer_cache_hit_connect(),
            }
            #[cfg(feature = "metrics")]
            counter!("dht_metadata_peer_failure_cache_hits_total", "reason" => reason.as_str())
                .increment(1);
            #[cfg(not(feature = "metrics"))]
            let _ = reason;
            return MetadataFetchOutcome::SkippedCached;
        }

        on_attempt();
        self.runtime_stats.metadata_peer_attempt();
        #[cfg(feature = "metrics")]
        {
            counter!("dht_metadata_fetch_attempts_total").increment(1);
            counter!("dht_metadata_peer_attempts_total").increment(1);
        }

        let started = Instant::now();
        let result = timeout(
            self.total_timeout,
            self.fetch_with_peer(info_hash, peer_addr),
        )
        .await;

        #[cfg(feature = "metrics")]
        histogram!("dht_metadata_fetch_duration_seconds").record(started.elapsed().as_secs_f64());
        self.runtime_stats.observe_metadata_fetch_duration(
            started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        );

        match result {
            Ok(Ok(metadata)) => {
                let cache_entries = self.peer_failure_cache.remove(&peer_addr, Instant::now());
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
                    MetadataFetchFailure::Other => self.runtime_stats.metadata_failure_other(),
                }
                #[cfg(feature = "metrics")]
                counter!("dht_metadata_fetch_result_total", "result" => "failed").increment(1);
                MetadataFetchOutcome::Failed
            }
            Err(_) => {
                self.record_peer_failure(peer_addr, PeerFailureReason::Timeout);
                self.runtime_stats.metadata_peer_failed();
                self.runtime_stats.metadata_peer_timeout();
                self.runtime_stats.metadata_failure_timeout();
                #[cfg(feature = "metrics")]
                {
                    counter!("dht_metadata_fetch_fail_total", "reason" => "timeout").increment(1);
                    counter!("dht_metadata_fetch_result_total", "result" => "timeout").increment(1);
                }
                MetadataFetchOutcome::Failed
            }
        }
    }

    fn record_peer_failure(&self, peer_addr: SocketAddr, reason: PeerFailureReason) {
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

    async fn fetch_with_peer(
        &self,
        info_hash: &[u8; 20],
        peer_addr: SocketAddr,
    ) -> Result<FetchedMetadata, MetadataFetchFailure> {
        let peer_id = PeerId::generate();
        let mut conn = match PeerConnection::connect(peer_addr, *info_hash, *peer_id.as_bytes())
            .await
        {
            Ok(conn) => {
                #[cfg(feature = "metrics")]
                counter!("dht_metadata_connection_result_total", "result" => "success")
                    .increment(1);
                conn
            }
            Err(_) => {
                self.record_peer_failure(peer_addr, PeerFailureReason::ConnectFailed);
                self.runtime_stats.metadata_connect_failed();
                #[cfg(feature = "metrics")]
                counter!("dht_metadata_connection_result_total", "result" => "failed").increment(1);
                return Err(MetadataFetchFailure::Connect);
            }
        };

        if !conn.supports_extension {
            self.runtime_stats.metadata_no_extension();
            #[cfg(feature = "metrics")]
            counter!("dht_metadata_handshake_result_total", "result" => "no_extension_support")
                .increment(1);
            return Err(MetadataFetchFailure::NoExtension);
        }

        let my_ut_metadata_id = 1;
        let handshake = ExtensionHandshake::with_extensions(&[("ut_metadata", my_ut_metadata_id)]);
        let handshake_bytes = handshake.encode().map_err(|_| MetadataFetchFailure::Send)?;
        if conn
            .send(Message::Extended {
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
            let msg = conn
                .receive()
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
                    for piece in 0..count {
                        let encoded = MetadataMessage::request(piece as u32)
                            .encode()
                            .map_err(|_| MetadataFetchFailure::Send)?;
                        if conn
                            .send(Message::Extended {
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
    use tokio::net::TcpListener;

    #[test]
    fn peer_failure_cache_is_socket_specific_and_expires() {
        let start = Instant::now();
        let cache = PeerFailureCache::new(10, Duration::from_secs(60));
        let first: SocketAddr = "127.0.0.1:1000".parse().unwrap();
        let same_ip_other_port: SocketAddr = "127.0.0.1:1001".parse().unwrap();

        assert_eq!(cache.insert(first, PeerFailureReason::Timeout, start), 1);
        assert_eq!(cache.get(first, start).0, Some(PeerFailureReason::Timeout));
        assert_eq!(cache.get(same_ip_other_port, start).0, None);
        assert_eq!(cache.get(first, start + Duration::from_secs(61)), (None, 0));
    }

    #[test]
    fn peer_failure_cache_evicts_oldest_at_capacity() {
        let start = Instant::now();
        let cache = PeerFailureCache::new(1, Duration::from_secs(60));
        let first: SocketAddr = "127.0.0.1:1000".parse().unwrap();
        let second: SocketAddr = "127.0.0.1:1001".parse().unwrap();

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
            fetcher.fetch(&[8; 20], addr).await,
            MetadataFetchOutcome::SkippedCached
        ));
        assert!(cached_started.elapsed() < Duration::from_millis(100));

        let snapshot = stats.snapshot();
        assert_eq!(snapshot.metadata_peer_attempts, 1);
        assert_eq!(snapshot.metadata_peer_failed, 1);
        assert_eq!(snapshot.metadata_peer_timeouts, 1);
        assert_eq!(snapshot.metadata_peer_failure_cache_hits, 1);
        assert_eq!(snapshot.metadata_peer_failure_cache_entries, 1);

        accept_task.abort();
    }
}
