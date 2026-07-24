use crate::addr::{addr_allowed_by_netmode, is_valid_node_addr};
use crate::node_id::TransactionId;
use crate::protocol::DhtResponse;
use crate::types::{NetMode, NodeTuple};
use bytes::BytesMut;
#[cfg(feature = "metrics")]
use metrics::{counter, histogram};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use tokio::net::UdpSocket;

pub(crate) fn for_each_response_node(
    response: &DhtResponse,
    netmode: NetMode,
    mut visit: impl FnMut(NodeTuple),
) -> usize {
    let mut count = 0;
    if netmode != NetMode::Ipv6Only
        && let Some(nodes) = response.nodes.as_deref()
        && nodes.len() % 26 == 0
    {
        for chunk in nodes.chunks_exact(26) {
            let id: [u8; 20] = chunk[..20].try_into().expect("compact v4 id is 20 bytes");
            let ip = Ipv4Addr::new(chunk[20], chunk[21], chunk[22], chunk[23]);
            let port = u16::from_be_bytes([chunk[24], chunk[25]]);
            let addr = SocketAddr::new(IpAddr::V4(ip), port);
            if is_valid_node_addr(&addr) {
                visit(NodeTuple { id, addr });
                count += 1;
            }
        }
    }
    if netmode != NetMode::Ipv4Only
        && let Some(nodes) = response.nodes6.as_deref()
        && nodes.len() % 38 == 0
    {
        for chunk in nodes.chunks_exact(38) {
            let id: [u8; 20] = chunk[..20].try_into().expect("compact v6 id is 20 bytes");
            let ip_bytes: [u8; 16] = chunk[20..36]
                .try_into()
                .expect("compact v6 address is 16 bytes");
            let port = u16::from_be_bytes([chunk[36], chunk[37]]);
            let addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip_bytes)), port);
            if is_valid_node_addr(&addr) {
                visit(NodeTuple { id, addr });
                count += 1;
            }
        }
    }
    count
}

pub(crate) fn for_each_response_peer(
    response: &DhtResponse,
    netmode: NetMode,
    mut visit: impl FnMut(SocketAddr),
) -> usize {
    let mut count = 0;
    let Some(values) = response.values.as_ref() else {
        return count;
    };
    for value in values {
        let bytes = value.as_ref();
        let addr = match bytes.len() {
            6 => {
                let ip = Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]);
                let port = u16::from_be_bytes([bytes[4], bytes[5]]);
                SocketAddr::new(IpAddr::V4(ip), port)
            }
            18 => {
                let ip_bytes: [u8; 16] = bytes[..16]
                    .try_into()
                    .expect("compact IPv6 Peer address is 16 bytes");
                let port = u16::from_be_bytes([bytes[16], bytes[17]]);
                SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip_bytes)), port)
            }
            _ => continue,
        };
        if addr_allowed_by_netmode(&addr, netmode) && is_valid_node_addr(&addr) {
            visit(addr);
            count += 1;
        }
    }
    count
}

pub(crate) fn encode_find_node_query(
    buffer: &mut BytesMut,
    tid: &TransactionId,
    target: &[u8; 20],
    sender_id: &[u8; 20],
) {
    buffer.clear();
    buffer.reserve(112);
    buffer.extend_from_slice(b"d1:ad2:id20:");
    buffer.extend_from_slice(sender_id);
    buffer.extend_from_slice(b"6:target20:");
    buffer.extend_from_slice(target);
    buffer.extend_from_slice(b"e1:q9:find_node1:t8:");
    buffer.extend_from_slice(tid);
    buffer.extend_from_slice(b"1:y1:qe");
}

pub(crate) fn encode_get_peers_query(
    buffer: &mut BytesMut,
    tid: &TransactionId,
    info_hash: &[u8; 20],
    sender_id: &[u8; 20],
) {
    buffer.clear();
    buffer.reserve(111);
    buffer.extend_from_slice(b"d1:ad2:id20:");
    buffer.extend_from_slice(sender_id);
    buffer.extend_from_slice(b"9:info_hash20:");
    buffer.extend_from_slice(info_hash);
    buffer.extend_from_slice(b"e1:q9:get_peers1:t8:");
    buffer.extend_from_slice(tid);
    buffer.extend_from_slice(b"1:y1:qe");
}

pub(crate) fn encode_response(
    buffer: &mut BytesMut,
    tid: &[u8],
    node_id: &[u8; 20],
    token: &[u8; 8],
    nodes: &[NodeTuple],
    ipv6: bool,
) {
    buffer.clear();
    buffer.reserve(384);
    buffer.extend_from_slice(b"d1:rd2:id20:");
    buffer.extend_from_slice(node_id);

    let compact_len = if ipv6 {
        nodes.iter().filter(|node| node.addr.is_ipv6()).count() * 38
    } else {
        nodes.iter().filter(|node| node.addr.is_ipv4()).count() * 26
    };
    if compact_len > 0 {
        if ipv6 {
            buffer.extend_from_slice(b"6:nodes6");
        } else {
            buffer.extend_from_slice(b"5:nodes");
        }
        push_usize(buffer, compact_len);
        buffer.extend_from_slice(b":");
        for node in nodes {
            match node.addr.ip() {
                IpAddr::V4(ip) if !ipv6 => {
                    buffer.extend_from_slice(&node.id);
                    buffer.extend_from_slice(&ip.octets());
                    buffer.extend_from_slice(&node.addr.port().to_be_bytes());
                }
                IpAddr::V6(ip) if ipv6 => {
                    buffer.extend_from_slice(&node.id);
                    buffer.extend_from_slice(&ip.octets());
                    buffer.extend_from_slice(&node.addr.port().to_be_bytes());
                }
                _ => {}
            }
        }
    }

    buffer.extend_from_slice(b"5:token8:");
    buffer.extend_from_slice(token);
    buffer.extend_from_slice(b"e1:t");
    push_usize(buffer, tid.len());
    buffer.extend_from_slice(b":");
    buffer.extend_from_slice(tid);
    buffer.extend_from_slice(b"1:y1:re");
}

fn push_usize(buffer: &mut BytesMut, mut value: usize) {
    let mut digits = [0u8; 20];
    let mut cursor = digits.len();
    loop {
        cursor -= 1;
        digits[cursor] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    buffer.extend_from_slice(&digits[cursor..]);
}

pub(crate) async fn send_find_node_query(
    addr: &SocketAddr,
    tid: &TransactionId,
    target: &[u8; 20],
    sender_id: &[u8; 20],
    socket: &Arc<UdpSocket>,
    buffer: &mut BytesMut,
) -> bool {
    encode_find_node_query(buffer, tid, target, sender_id);
    match socket.send_to(buffer, addr).await {
        Ok(len) => {
            #[cfg(feature = "metrics")]
            {
                counter!("dht_udp_bytes_sent_total").increment(len as u64);
                counter!("dht_udp_packets_sent_total", "type" => "query").increment(1);
                histogram!("dht_udp_query_size_bytes").record(len as f64);
            }
            #[cfg(not(feature = "metrics"))]
            let _ = len;
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::DhtMessage;

    #[test]
    fn manual_find_node_encoding_round_trips() {
        let mut buffer = BytesMut::new();
        let tid = [1; 8];
        let target = [2; 20];
        let sender = [3; 20];
        encode_find_node_query(&mut buffer, &tid, &target, &sender);
        let message: DhtMessage = serde_bencode::from_bytes(&buffer).unwrap();
        assert_eq!(message.t.as_ref(), &tid);
        assert_eq!(message.q.as_deref(), Some("find_node"));
        assert_eq!(message.a.unwrap().target.unwrap().as_ref(), &target);
    }

    #[test]
    fn manual_get_peers_encoding_round_trips() {
        let mut buffer = BytesMut::new();
        let tid = [1; 8];
        let info_hash = [2; 20];
        let sender = [3; 20];
        encode_get_peers_query(&mut buffer, &tid, &info_hash, &sender);
        let message: DhtMessage = serde_bencode::from_bytes(&buffer).unwrap();
        assert_eq!(message.t.as_ref(), &tid);
        assert_eq!(message.q.as_deref(), Some("get_peers"));
        assert_eq!(message.a.unwrap().info_hash.unwrap().as_ref(), &info_hash);
    }

    #[test]
    fn manual_response_encoding_round_trips() {
        let mut buffer = BytesMut::new();
        let nodes = [NodeTuple {
            id: [4; 20],
            addr: "8.8.8.8:6881".parse().unwrap(),
        }];
        encode_response(&mut buffer, &[1, 2], &[2; 20], &[3; 8], &nodes, false);
        let message: DhtMessage = serde_bencode::from_bytes(&buffer).unwrap();
        let response = message.r.unwrap();
        assert_eq!(response.nodes.unwrap().len(), 26);
    }

    #[test]
    fn compact_get_peers_values_are_validated() {
        let response = DhtResponse {
            id: None,
            nodes: None,
            nodes6: None,
            values: Some(vec![
                serde_bytes::ByteBuf::from(vec![8, 8, 8, 8, 0x1a, 0xe1]),
                serde_bytes::ByteBuf::from(vec![10, 0, 0, 1, 0x1a, 0xe1]),
                serde_bytes::ByteBuf::from(vec![1, 2, 3]),
            ]),
        };
        let mut peers = Vec::new();
        assert_eq!(
            for_each_response_peer(&response, NetMode::Ipv4Only, |peer| peers.push(peer)),
            1
        );
        assert_eq!(peers[0], "8.8.8.8:6881".parse().unwrap());
    }
}
