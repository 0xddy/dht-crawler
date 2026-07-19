use crate::error::DHTError;
use crate::runtime_stats::DhtRuntimeStats;
use crate::udp_buffer::{MAX_DHT_UDP_PACKET, UdpBufferPool, UdpPacket};
#[cfg(feature = "metrics")]
use metrics::counter;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub(crate) type WorkerHandle = mpsc::Sender<(UdpPacket, SocketAddr, SocketAddr)>;

pub(crate) fn spawn_udp_listener(
    socket: Arc<UdpSocket>,
    mut workers: Vec<WorkerHandle>,
    shutdown: CancellationToken,
    buffer_pool: UdpBufferPool,
    runtime_stats: DhtRuntimeStats,
) -> crate::error::Result<()> {
    let local_addr = socket
        .local_addr()
        .map_err(|e| DHTError::Init(format!("socket local addr failed: {e}")))?;
    if workers.is_empty() {
        return Err(DHTError::Init(
            "spawn_udp_listener: no worker provided".to_string(),
        ));
    }
    tokio::spawn(async move {
        loop {
            let mut buf = buffer_pool.acquire();
            let recv_buf = &mut buf[..buffer_pool.buf_capacity()];

            tokio::select! {
                _ = shutdown.cancelled() => {
                    buffer_pool.release(buf);
                    break;
                }
                result = socket.recv_from(recv_buf) => {
                    match result {
                        Ok((size, origin_addr)) => {
                            if let Err(ProcessUdpPacketError::NoLiveWorkers) =
                                process_udp_packet(buf, size, origin_addr, local_addr, &buffer_pool, &runtime_stats, &mut workers)
                            {
                                log::warn!("Socket {socket:?} is closing because no worker can process packets.");
                                break
                            }
                        }
                        Err(_) => {
                            buffer_pool.release(buf);
                            tokio::select! {
                                _ = shutdown.cancelled() => break,
                                _ = tokio::time::sleep(Duration::from_millis(1)) => {},
                            }
                        }
                    }
                }
            }
        }
    });
    Ok(())
}

enum ProcessUdpPacketError {
    PacketTooLarge,
    InvalidPacket,
    ChokedWorkers,
    NoLiveWorkers,
}

fn process_udp_packet(
    buf: Box<[u8]>,
    size: usize,
    origin_addr: SocketAddr,
    local_addr: SocketAddr,
    buffer_pool: &UdpBufferPool,
    runtime_stats: &DhtRuntimeStats,
    workers: &mut Vec<WorkerHandle>,
) -> std::result::Result<(), ProcessUdpPacketError> {
    runtime_stats.udp_received();
    runtime_stats.udp_received_bytes(size);
    #[cfg(feature = "metrics")]
    counter!("dht_udp_bytes_received_total").increment(size as u64);

    if size > MAX_DHT_UDP_PACKET {
        runtime_stats.udp_invalid();
        #[cfg(feature = "metrics")]
        counter!("dht_udp_packets_received_total", "status" => "dropped_size").increment(1);
        buffer_pool.release(buf);
        return Err(ProcessUdpPacketError::PacketTooLarge);
    }

    if size == 0 || buf[0] != b'd' {
        runtime_stats.udp_invalid();
        #[cfg(feature = "metrics")]
        counter!("dht_udp_packets_received_total", "status" => "dropped_magic").increment(1);
        buffer_pool.release(buf);
        return Err(ProcessUdpPacketError::InvalidPacket);
    }

    let mut packet = UdpPacket { buf, len: size };
    let mut hasher = ahash::AHasher::default();
    origin_addr.hash(&mut hasher);
    let origin_hash = hasher.finish() as usize;

    'select_worker: loop {
        if workers.is_empty() {
            buffer_pool.release(packet.buf);
            return Err(ProcessUdpPacketError::NoLiveWorkers);
        }

        let worker_count = workers.len();
        let preferred_index = origin_hash % worker_count;
        for offset in 0..worker_count {
            let worker_index = (preferred_index + offset) % worker_count;
            match workers[worker_index].try_send((packet, origin_addr, local_addr)) {
                Ok(_) => {
                    #[cfg(feature = "metrics")]
                    counter!("dht_udp_packets_received_total", "status" => "ok").increment(1);
                    return Ok(());
                }
                Err(mpsc::error::TrySendError::Full((p, _, _))) => {
                    packet = p;
                }
                Err(mpsc::error::TrySendError::Closed((p, _, _))) => {
                    packet = p;
                    log::warn!("UDP worker dropped.");
                    workers.swap_remove(worker_index);
                    continue 'select_worker;
                }
            }
        }

        #[cfg(feature = "metrics")]
        counter!("dht_udp_packets_received_total", "status" => "queue_full").increment(1);
        runtime_stats.udp_queue_full();
        buffer_pool.release(packet.buf);
        return Err(ProcessUdpPacketError::ChokedWorkers);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addresses() -> (SocketAddr, SocketAddr) {
        (
            "8.8.8.8:6881".parse().unwrap(),
            "0.0.0.0:12313".parse().unwrap(),
        )
    }

    fn buffer(pool: &UdpBufferPool, first: u8) -> Box<[u8]> {
        let mut buf = pool.acquire();
        buf[0] = first;
        buf
    }

    fn packet(pool: &UdpBufferPool, first: u8) -> UdpPacket {
        UdpPacket {
            buf: buffer(pool, first),
            len: 1,
        }
    }

    fn preferred_index(origin_addr: SocketAddr, worker_count: usize) -> usize {
        let mut hasher = ahash::AHasher::default();
        origin_addr.hash(&mut hasher);
        (hasher.finish() as usize) % worker_count
    }

    #[test]
    fn available_preferred_worker_is_used_first() {
        let pool = UdpBufferPool::new();
        let stats = DhtRuntimeStats::default();
        let (origin_addr, local_addr) = addresses();
        let (tx0, mut rx0) = mpsc::channel(1);
        let (tx1, mut rx1) = mpsc::channel(1);
        let mut workers = vec![tx0, tx1];
        let preferred = preferred_index(origin_addr, workers.len());

        assert!(
            process_udp_packet(
                buffer(&pool, b'd'),
                1,
                origin_addr,
                local_addr,
                &pool,
                &stats,
                &mut workers,
            )
            .is_ok()
        );

        let (preferred_rx, fallback_rx) = if preferred == 0 {
            (&mut rx0, &mut rx1)
        } else {
            (&mut rx1, &mut rx0)
        };
        let (forwarded, _, _) = preferred_rx.try_recv().unwrap();
        assert_eq!(forwarded.payload(), b"d");
        assert!(matches!(
            fallback_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        pool.release(forwarded.buf);
    }

    #[test]
    fn full_preferred_worker_falls_back_to_available_worker() {
        let pool = UdpBufferPool::new();
        let stats = DhtRuntimeStats::default();
        let (origin_addr, local_addr) = addresses();
        let (tx0, mut rx0) = mpsc::channel(1);
        let (tx1, mut rx1) = mpsc::channel(1);
        let mut workers = vec![tx0, tx1];
        let preferred = preferred_index(origin_addr, workers.len());

        workers[preferred]
            .try_send((packet(&pool, b'x'), origin_addr, local_addr))
            .unwrap();

        assert!(
            process_udp_packet(
                buffer(&pool, b'd'),
                1,
                origin_addr,
                local_addr,
                &pool,
                &stats,
                &mut workers,
            )
            .is_ok()
        );

        let (preferred_rx, fallback_rx) = if preferred == 0 {
            (&mut rx0, &mut rx1)
        } else {
            (&mut rx1, &mut rx0)
        };
        let (queued, _, _) = preferred_rx.try_recv().unwrap();
        let (forwarded, _, _) = fallback_rx.try_recv().unwrap();
        assert_eq!(queued.payload(), b"x");
        assert_eq!(forwarded.payload(), b"d");
        pool.release(queued.buf);
        pool.release(forwarded.buf);
    }

    #[test]
    fn closed_preferred_worker_is_removed_before_fallback() {
        let pool = UdpBufferPool::new();
        let stats = DhtRuntimeStats::default();
        let (origin_addr, local_addr) = addresses();
        let (closed_tx, closed_rx) = mpsc::channel(1);
        drop(closed_rx);
        let (open_tx, mut open_rx) = mpsc::channel(1);
        let preferred = preferred_index(origin_addr, 2);
        let mut workers = if preferred == 0 {
            vec![closed_tx, open_tx]
        } else {
            vec![open_tx, closed_tx]
        };

        assert!(
            process_udp_packet(
                buffer(&pool, b'd'),
                1,
                origin_addr,
                local_addr,
                &pool,
                &stats,
                &mut workers,
            )
            .is_ok()
        );

        assert_eq!(workers.len(), 1);
        let (forwarded, _, _) = open_rx.try_recv().unwrap();
        assert_eq!(forwarded.payload(), b"d");
        pool.release(forwarded.buf);
    }

    #[test]
    fn packet_is_dropped_only_after_all_live_workers_are_full() {
        let pool = UdpBufferPool::new();
        let stats = DhtRuntimeStats::default();
        let (origin_addr, local_addr) = addresses();
        let (tx0, mut rx0) = mpsc::channel(1);
        let (tx1, mut rx1) = mpsc::channel(1);
        let mut workers = vec![tx0, tx1];
        for worker in &workers {
            worker
                .try_send((packet(&pool, b'x'), origin_addr, local_addr))
                .unwrap();
        }

        let result = process_udp_packet(
            buffer(&pool, b'd'),
            1,
            origin_addr,
            local_addr,
            &pool,
            &stats,
            &mut workers,
        );

        assert!(matches!(result, Err(ProcessUdpPacketError::ChokedWorkers)));
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.udp_received, 1);
        assert_eq!(snapshot.udp_queue_full, 1);
        assert_eq!(snapshot.udp_invalid, 0);
        assert_eq!(workers.len(), 2);
        for receiver in [&mut rx0, &mut rx1] {
            let (queued, _, _) = receiver.try_recv().unwrap();
            assert_eq!(queued.payload(), b"x");
            pool.release(queued.buf);
        }
    }

    #[test]
    fn invalid_packet_updates_runtime_stats() {
        let pool = UdpBufferPool::new();
        let stats = DhtRuntimeStats::default();
        let (origin_addr, local_addr) = addresses();
        let mut workers = Vec::new();

        let result = process_udp_packet(
            buffer(&pool, b'x'),
            1,
            origin_addr,
            local_addr,
            &pool,
            &stats,
            &mut workers,
        );

        assert!(matches!(result, Err(ProcessUdpPacketError::InvalidPacket)));
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.udp_received, 1);
        assert_eq!(snapshot.udp_invalid, 1);
        assert_eq!(snapshot.udp_queue_full, 0);
    }
}
