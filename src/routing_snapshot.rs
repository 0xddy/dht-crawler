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
}
