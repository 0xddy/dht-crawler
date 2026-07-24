use crate::types::NodeTuple;
use rand::seq::SliceRandom;

#[derive(Default)]
pub(crate) struct RoutingSnapshot {
    v4: Vec<NodeTuple>,
    v6: Vec<NodeTuple>,
}

impl RoutingSnapshot {
    pub(crate) fn from_nodes(nodes: Vec<NodeTuple>, limit: usize) -> Self {
        let mut v4 = Vec::new();
        let mut v6 = Vec::new();
        for node in nodes.into_iter().take(limit) {
            if node.addr.is_ipv6() {
                v6.push(node);
            } else {
                v4.push(node);
            }
        }
        Self { v4, v6 }
    }

    pub(crate) fn random_nodes(&self, count: usize, filter_ipv6: Option<bool>) -> Vec<NodeTuple> {
        let mut rng = rand::thread_rng();
        match filter_ipv6 {
            Some(true) => self.v6.choose_multiple(&mut rng, count).cloned().collect(),
            Some(false) => self.v4.choose_multiple(&mut rng, count).cloned().collect(),
            None => {
                let mut all = Vec::with_capacity(self.v4.len() + self.v6.len());
                all.extend_from_slice(&self.v4);
                all.extend_from_slice(&self.v6);
                all.choose_multiple(&mut rng, count).cloned().collect()
            }
        }
    }

    pub(crate) fn closest_nodes(
        &self,
        target: &[u8; 20],
        count: usize,
        filter_ipv6: Option<bool>,
    ) -> Vec<NodeTuple> {
        if count == 0 {
            return Vec::new();
        }
        let mut nodes = match filter_ipv6 {
            Some(true) => self.v6.clone(),
            Some(false) => self.v4.clone(),
            None => {
                let mut all = Vec::with_capacity(self.v4.len() + self.v6.len());
                all.extend_from_slice(&self.v4);
                all.extend_from_slice(&self.v6);
                all
            }
        };
        let compare =
            |left: &NodeTuple, right: &NodeTuple| xor_distance_cmp(&left.id, &right.id, target);
        if nodes.len() > count {
            nodes.select_nth_unstable_by(count, compare);
            nodes.truncate(count);
        }
        nodes.sort_unstable_by(compare);
        nodes
    }
}

pub(crate) fn xor_distance_cmp(
    left: &[u8; 20],
    right: &[u8; 20],
    target: &[u8; 20],
) -> std::cmp::Ordering {
    for index in 0..20 {
        let ordering = (left[index] ^ target[index]).cmp(&(right[index] ^ target[index]));
        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }
    std::cmp::Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn node(id: u8, port: u16) -> NodeTuple {
        NodeTuple {
            id: [id; 20],
            addr: SocketAddr::from(([8, 8, 8, 8], port)),
        }
    }

    #[test]
    fn closest_nodes_orders_by_xor_distance_and_limits_results() {
        let snapshot =
            RoutingSnapshot::from_nodes(vec![node(0xf0, 1), node(0x01, 2), node(0x10, 3)], 3);
        let closest = snapshot.closest_nodes(&[0; 20], 2, Some(false));
        assert_eq!(
            closest
                .iter()
                .map(|node| node.addr.port())
                .collect::<Vec<_>>(),
            vec![2, 3]
        );
    }
}
