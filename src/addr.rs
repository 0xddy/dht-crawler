use crate::types::NetMode;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub(crate) fn addr_allowed_by_netmode(addr: &SocketAddr, netmode: NetMode) -> bool {
    match netmode {
        NetMode::Ipv4Only => addr.is_ipv4(),
        NetMode::Ipv6Only => addr.is_ipv6(),
        NetMode::DualStack => true,
    }
}

pub(crate) fn is_valid_node_addr(addr: &SocketAddr) -> bool {
    if addr.port() == 0 {
        return false;
    }

    match addr.ip() {
        IpAddr::V4(ip) => is_valid_ipv4_node_addr(ip),
        IpAddr::V6(ip) => is_valid_ipv6_node_addr(ip),
    }
}

fn is_valid_ipv4_node_addr(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    let is_cgnat = octets[0] == 100 && (octets[1] & 0b1100_0000) == 64;
    let is_benchmark = octets[0] == 198 && (octets[1] == 18 || octets[1] == 19);
    let is_reserved = octets[0] >= 240;

    !ip.is_unspecified()
        && !ip.is_loopback()
        && !ip.is_private()
        && !ip.is_link_local()
        && !ip.is_multicast()
        && !ip.is_broadcast()
        && !ip.is_documentation()
        && !is_cgnat
        && !is_benchmark
        && !is_reserved
}

fn is_valid_ipv6_node_addr(ip: Ipv6Addr) -> bool {
    let octets = ip.octets();
    let is_unique_local = (octets[0] & 0xfe) == 0xfc;
    let is_unicast_link_local = octets[0] == 0xfe && (octets[1] & 0xc0) == 0x80;
    let is_documentation =
        octets[0] == 0x20 && octets[1] == 0x01 && octets[2] == 0x0d && octets[3] == 0xb8;

    !ip.is_unspecified()
        && !ip.is_loopback()
        && !ip.is_multicast()
        && !is_unique_local
        && !is_unicast_link_local
        && !is_documentation
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_node_addresses_are_filtered() {
        assert!(is_valid_node_addr(&"8.8.8.8:6881".parse().unwrap()));
        assert!(is_valid_node_addr(
            &"[2001:4860:4860::8888]:6881".parse().unwrap()
        ));
        assert!(!is_valid_node_addr(&"8.8.8.8:0".parse().unwrap()));
        assert!(!is_valid_node_addr(&"10.0.0.1:6881".parse().unwrap()));
        assert!(!is_valid_node_addr(&"127.0.0.1:6881".parse().unwrap()));
        assert!(!is_valid_node_addr(&"[fc00::1]:6881".parse().unwrap()));
    }
}
