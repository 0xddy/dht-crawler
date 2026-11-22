use super::queue::PeerCandidate;
use super::{MAX_METADATA_CANDIDATES_PER_HASH, MAX_METADATA_PEERS_PER_HASH};
use crate::metadata::{FetchedMetadata, MetadataFetchOutcome};
use crate::peer_lookup::PeerLookupRequest;
use crate::runtime_stats::DhtRuntimeStats;
use crate::server::HashSource;
use crate::types::NodeTuple;
#[cfg(feature = "metrics")]
use metrics::counter;
use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

const LIVE_PEER_IDLE_GRACE: Duration = Duration::from_millis(300);
const ACTIVE_PEER_LOOKUP_DELAY: Duration = Duration::from_millis(100);
pub(super) const MAX_LOOKUP_DURATION: Duration = Duration::from_secs(300);

pub(super) struct JobPeerLookup {
    pub(super) request_tx: mpsc::Sender<PeerLookupRequest>,
    pub(super) info_hash: [u8; 20],
    pub(super) source_nodes: Vec<NodeTuple>,
    pub(super) source: HashSource,
    pub(super) timeout: Duration,
}

fn queue_race_candidate(
    peer: PeerCandidate,
    source: &'static str,
    seen: &mut HashSet<SocketAddr>,
    pending: &mut VecDeque<(PeerCandidate, &'static str)>,
) {
    if seen.len() < MAX_METADATA_CANDIDATES_PER_HASH && seen.insert(peer.addr) {
        pending.push_back((peer, source));
    }
}

fn submit_pending_lookup(
    pending_lookup: &mut Option<(JobPeerLookup, mpsc::Sender<SocketAddr>)>,
    lookup_submitted: &mut bool,
    runtime_stats: &DhtRuntimeStats,
) {
    *lookup_submitted = true;
    if let Some((lookup, peer_tx)) = pending_lookup.take()
        && lookup
            .request_tx
            .try_send(PeerLookupRequest {
                info_hash: lookup.info_hash,
                peer_tx,
                source_nodes: lookup.source_nodes,
                source: lookup.source,
            })
            .is_err()
    {
        runtime_stats.yield_stats().inc("lookup_request_queue_full");
    }
}

pub(super) async fn race_peer_fetches<F, Fut>(
    peers: Vec<PeerCandidate>,
    peer_rx: &mut mpsc::Receiver<PeerCandidate>,
    lookup: Option<JobPeerLookup>,
    fetch: F,
    runtime_stats: &DhtRuntimeStats,
) -> Option<(PeerCandidate, FetchedMetadata)>
where
    F: Fn(PeerCandidate) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = (PeerCandidate, MetadataFetchOutcome)> + Send + 'static,
{
    runtime_stats.metadata_race_started();
    #[cfg(feature = "metrics")]
    counter!("dht_metadata_peer_races_total").increment(1);

    let mut tasks = JoinSet::new();
    let mut seen_peers = HashSet::new();
    let mut pending_peers = VecDeque::new();
    for peer in peers {
        queue_race_candidate(peer, "initial", &mut seen_peers, &mut pending_peers);
    }
    // Reserve one potential attempt for every outstanding fetch. A cache skip refunds
    // its reservation, while any real attempt keeps it, even after a fast failure.
    // This bounds both simultaneous TCP connections and total actual attempts to 12.
    let mut attempts_reserved = 0usize;

    let mut lookup_open = lookup.is_some();
    let lookup_timeout = lookup.as_ref().map_or(Duration::ZERO, |lookup| {
        lookup.timeout.min(MAX_LOOKUP_DURATION)
    });
    let mut lookup_submitted = !lookup_open;
    let (lookup_peer_tx, mut lookup_peer_rx) = mpsc::channel(MAX_METADATA_CANDIDATES_PER_HASH);
    let mut pending_lookup = lookup.map(|lookup| (lookup, lookup_peer_tx));
    let delay = if pending_peers.is_empty() {
        Duration::ZERO
    } else {
        ACTIVE_PEER_LOOKUP_DELAY
    };
    let submit_delay = tokio::time::sleep_until(tokio::time::Instant::now() + delay);
    tokio::pin!(submit_delay);
    // Includes request-channel delay and a small scheduling allowance. The actor has its
    // own deadline too, but a stalled or cancelled actor must never hold a job indefinitely.
    let lookup_deadline =
        tokio::time::Instant::now() + delay + lookup_timeout + LIVE_PEER_IDLE_GRACE;
    let mut lookup_deadline_elapsed = false;
    let mut accepting_live_peers = true;
    loop {
        while attempts_reserved < MAX_METADATA_PEERS_PER_HASH {
            let Some((peer, source)) = pending_peers.pop_front() else {
                break;
            };
            attempts_reserved += 1;
            tasks.spawn(fetch.clone()(peer));
            runtime_stats.metadata_peer_candidate();
            if source != "initial" {
                runtime_stats.metadata_live_peer_join();
                #[cfg(feature = "metrics")]
                counter!("dht_metadata_live_peer_joins_total").increment(1);
            }
            #[cfg(feature = "metrics")]
            counter!("dht_metadata_peer_candidates_total", "source" => source).increment(1);
        }
        if seen_peers.len() >= MAX_METADATA_CANDIDATES_PER_HASH {
            accepting_live_peers = false;
            lookup_open = false;
            pending_lookup = None;
            peer_rx.close();
            lookup_peer_rx.close();
        }
        if lookup_open && !lookup_submitted && tasks.is_empty() && pending_peers.is_empty() {
            submit_pending_lookup(&mut pending_lookup, &mut lookup_submitted, runtime_stats);
        }
        if tasks.is_empty()
            && (attempts_reserved >= MAX_METADATA_PEERS_PER_HASH
                || (pending_peers.is_empty() && !accepting_live_peers && !lookup_open))
        {
            return None;
        }

        tokio::select! {
            result = tasks.join_next(), if !tasks.is_empty() => {
                let Some(Ok((peer, outcome))) = result else { continue; };
                match outcome {
                    MetadataFetchOutcome::SkippedCached => {
                        attempts_reserved = attempts_reserved.saturating_sub(1);
                    }
                    MetadataFetchOutcome::Failed => {},
                    MetadataFetchOutcome::Fetched(metadata) => {
                        let canceled = tasks.len();
                        runtime_stats.metadata_peer_canceled(canceled);
                        #[cfg(feature = "metrics")]
                        counter!("dht_metadata_peer_canceled_total").increment(canceled as u64);
                        let _ = pending_lookup.take();
                        peer_rx.close();
                        lookup_peer_rx.close();
                        tasks.abort_all();
                        while tasks.join_next().await.is_some() {}
                        return Some((peer, metadata));
                    }
                }
            }
            peer = peer_rx.recv(), if accepting_live_peers => {
                match peer {
                    Some(peer) => queue_race_candidate(peer, "live", &mut seen_peers, &mut pending_peers),
                    None => accepting_live_peers = false,
                }
            }
            peer = lookup_peer_rx.recv(), if lookup_open => {
                match peer {
                    Some(addr) => queue_race_candidate(PeerCandidate { addr, discovered_at: Instant::now() }, "lookup", &mut seen_peers, &mut pending_peers),
                    None => lookup_open = false,
                }
            }
            _ = &mut submit_delay, if lookup_open && !lookup_submitted => {
                submit_pending_lookup(&mut pending_lookup, &mut lookup_submitted, runtime_stats);
            }
            _ = tokio::time::sleep_until(lookup_deadline),
                if lookup_open && !lookup_deadline_elapsed => {
                lookup_peer_rx.close();
                lookup_deadline_elapsed = true;
                // Drain any already-delivered peers before treating the lookup as complete.
            }
            _ = tokio::time::sleep(LIVE_PEER_IDLE_GRACE),
                if tasks.is_empty() && pending_peers.is_empty() && !lookup_open => return None,
        }
    }
}
