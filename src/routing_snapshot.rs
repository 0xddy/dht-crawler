use crate::types::NodeTuple;
use rand::seq::SliceRandom;

#[derive(Debug)]
enum PrefixBranch {
    Leaf { start: usize, end: usize },
    Split { bit: usize, zero: usize, one: usize },
}

/// An immutable Patricia trie over sorted IDs. Unary prefixes are compressed,
/// so construction is bounded by two branches per distinct ID.
#[derive(Default)]
struct PrefixIndex {
    nodes: Vec<NodeTuple>,
    branches: Vec<PrefixBranch>,
    root: Option<usize>,
}

impl PrefixIndex {
    fn new(mut nodes: Vec<NodeTuple>) -> Self {
        nodes.sort_unstable_by(|left, right| {
            left.id.cmp(&right.id).then(left.addr.cmp(&right.addr))
        });
        let mut index = Self {
            nodes,
            ..Self::default()
        };
        if !index.nodes.is_empty() {
            index.root = Some(index.build(0, index.nodes.len()));
        }
        index
    }

    fn build(&mut self, start: usize, end: usize) -> usize {
        let first = self.nodes[start].id;
        let last = self.nodes[end - 1].id;
        let branch = if let Some(byte) = first
            .iter()
            .zip(last)
            .position(|(left, right)| *left != right)
        {
            let bit = byte * 8 + (first[byte] ^ last[byte]).leading_zeros() as usize;
            let mask = 1 << (7 - bit % 8);
            let middle =
                start + self.nodes[start..end].partition_point(|node| node.id[byte] & mask == 0);
            let zero = self.build(start, middle);
            let one = self.build(middle, end);
            PrefixBranch::Split { bit, zero, one }
        } else {
            PrefixBranch::Leaf { start, end }
        };
        let position = self.branches.len();
        self.branches.push(branch);
        position
    }

    fn append_nearest(
        &self,
        target: &[u8; 20],
        count: usize,
        output: &mut Vec<NodeTuple>,
    ) -> usize {
        let Some(root) = self.root else {
            return 0;
        };
        let limit = output.len().saturating_add(count.min(self.nodes.len()));
        self.visit(root, target, limit, output)
    }

    fn visit(
        &self,
        branch: usize,
        target: &[u8; 20],
        limit: usize,
        output: &mut Vec<NodeTuple>,
    ) -> usize {
        if output.len() >= limit {
            return 0;
        }
        match self.branches[branch] {
            PrefixBranch::Leaf { start, end } => {
                output.extend_from_slice(&self.nodes[start..end.min(start + limit - output.len())]);
                1
            }
            PrefixBranch::Split { bit, zero, one } => {
                let (near, far) = if target[bit / 8] & (1 << (7 - bit % 8)) == 0 {
                    (zero, one)
                } else {
                    (one, zero)
                };
                1 + self.visit(near, target, limit, output) + self.visit(far, target, limit, output)
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct RoutingSnapshot {
    v4: PrefixIndex,
    v6: PrefixIndex,
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
        Self {
            v4: PrefixIndex::new(v4),
            v6: PrefixIndex::new(v6),
        }
    }

    pub(crate) fn random_nodes(&self, count: usize, filter_ipv6: Option<bool>) -> Vec<NodeTuple> {
        let mut rng = rand::thread_rng();
        match filter_ipv6 {
            Some(true) => self
                .v6
                .nodes
                .choose_multiple(&mut rng, count)
                .cloned()
                .collect(),
            Some(false) => self
                .v4
                .nodes
                .choose_multiple(&mut rng, count)
                .cloned()
                .collect(),
            None => {
                let v4_len = self.v4.nodes.len();
                let total = v4_len + self.v6.nodes.len();
                rand::seq::index::sample(&mut rng, total, count.min(total))
                    .iter()
                    .map(|index| {
                        if index < v4_len {
                            self.v4.nodes[index]
                        } else {
                            self.v6.nodes[index - v4_len]
                        }
                    })
                    .collect()
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
        let mut nodes = Vec::with_capacity(count.min(self.v4.nodes.len() + self.v6.nodes.len()));
        match filter_ipv6 {
            Some(true) => {
                self.v6.append_nearest(target, count, &mut nodes);
            }
            Some(false) => {
                self.v4.append_nearest(target, count, &mut nodes);
            }
            None => {
                self.v4.append_nearest(target, count, &mut nodes);
                self.v6.append_nearest(target, count, &mut nodes);
                // Only the at-most-2k winners need merging; never copy the full snapshot.
                nodes.sort_unstable_by(|left, right| {
                    xor_distance_cmp(&left.id, &right.id, target).then(left.addr.cmp(&right.addr))
                });
                nodes.truncate(count);
            }
        }
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
    use rand::{Rng, SeedableRng};
    use std::net::{Ipv6Addr, SocketAddr};

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

    #[test]
    fn prefix_index_matches_brute_force_for_mixed_families_and_duplicate_ids() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(0x1234);
        let mut nodes = Vec::new();
        for port in 1..=4096 {
            let mut id: [u8; 20] = rng.r#gen();
            if port % 3 == 0 {
                id[..18].fill(7);
            }
            if port % 13 == 0 {
                id = [9; 20];
            }
            let addr = if port % 2 == 0 {
                SocketAddr::from((Ipv6Addr::LOCALHOST, port))
            } else {
                SocketAddr::from(([8, 8, 8, 8], port))
            };
            nodes.push(NodeTuple { id, addr });
        }
        let snapshot = RoutingSnapshot::from_nodes(nodes.clone(), nodes.len());
        for _ in 0..40 {
            let target = rng.r#gen();
            for filter in [None, Some(false), Some(true)] {
                let mut expected: Vec<_> = nodes
                    .iter()
                    .copied()
                    .filter(|node| filter.is_none_or(|ipv6| node.addr.is_ipv6() == ipv6))
                    .collect();
                expected.sort_unstable_by(|left, right| {
                    xor_distance_cmp(&left.id, &right.id, &target).then(left.addr.cmp(&right.addr))
                });
                for count in [0, 1, 12, 128, usize::MAX] {
                    assert_eq!(
                        snapshot.closest_nodes(&target, count, filter),
                        expected[..count.min(expected.len())]
                    );
                }
            }
        }
    }

    #[test]
    fn nearest_query_visits_a_small_prefix_subset_and_copies_only_winners() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(83);
        let nodes: Vec<_> = (1..=8192)
            .map(|port| NodeTuple {
                id: rng.r#gen(),
                addr: SocketAddr::from(([8, 8, 8, 8], port)),
            })
            .collect();
        let index = PrefixIndex::new(nodes);
        let mut output = Vec::new();
        let visited = index.append_nearest(&[7; 20], 12, &mut output);
        assert_eq!(output.len(), 12);
        assert!(
            visited < 100,
            "visited {visited} branches for a 12-node result"
        );
        assert!(index.branches.len() <= index.nodes.len() * 2);
    }

    #[test]
    fn empty_and_identical_id_snapshots_keep_limits_and_random_samples_unique() {
        let empty = RoutingSnapshot::default();
        assert!(empty.closest_nodes(&[0; 20], usize::MAX, None).is_empty());
        assert!(empty.random_nodes(8, None).is_empty());
        let snapshot =
            RoutingSnapshot::from_nodes((1..=50).map(|port| node(1, port)).collect(), 20);
        assert_eq!(snapshot.closest_nodes(&[1; 20], 100, None).len(), 20);
        let chosen = snapshot.random_nodes(8, None);
        let distinct: std::collections::HashSet<_> = chosen.iter().map(|node| node.addr).collect();
        assert_eq!(chosen.len(), 8);
        assert_eq!(distinct.len(), 8);
        assert_eq!(snapshot.v4.branches.len(), 1);
    }

    #[test]
    #[ignore = "manual bounded comparison of prefix lookup against the previous full-copy algorithm"]
    fn compare_prefix_lookup_with_full_copy() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(1984);
        let nodes: Vec<_> = (0..98_304u32)
            .map(|index| NodeTuple {
                id: rng.r#gen(),
                addr: SocketAddr::from((
                    [8, 8, (index / 65535) as u8, 1],
                    (index % 65535 + 1) as u16,
                )),
            })
            .collect();
        let targets: Vec<[u8; 20]> = (0..200).map(|_| rng.r#gen()).collect();
        let build_start = std::time::Instant::now();
        let snapshot = RoutingSnapshot::from_nodes(nodes.clone(), nodes.len());
        let build_elapsed = build_start.elapsed();
        let indexed_start = std::time::Instant::now();
        let indexed: Vec<_> = targets
            .iter()
            .map(|target| snapshot.closest_nodes(std::hint::black_box(target), 12, Some(false)))
            .collect();
        let indexed_elapsed = indexed_start.elapsed();
        let baseline_start = std::time::Instant::now();
        for (target, actual) in targets.iter().zip(&indexed) {
            let mut expected = nodes.clone();
            let compare = |left: &NodeTuple, right: &NodeTuple| {
                xor_distance_cmp(&left.id, &right.id, target).then(left.addr.cmp(&right.addr))
            };
            expected.select_nth_unstable_by(12, compare);
            expected.truncate(12);
            expected.sort_unstable_by(compare);
            assert_eq!(*actual, expected);
        }
        eprintln!(
            "98,304 nodes / 200 lookups: build={build_elapsed:?}, indexed={indexed_elapsed:?}, full_copy={:?}",
            baseline_start.elapsed()
        );
    }
}
