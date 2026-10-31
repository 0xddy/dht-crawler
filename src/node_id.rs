use rand::Rng;

pub(crate) type TransactionId = [u8; 8];

pub(crate) fn transaction_id_from_bytes(bytes: &[u8]) -> Option<TransactionId> {
    if bytes.len() != 8 {
        return None;
    }

    let mut tid = [0u8; 8];
    tid.copy_from_slice(bytes);
    Some(tid)
}

pub(crate) fn random_node_id() -> [u8; 20] {
    let mut id = [0u8; 20];
    rand::thread_rng().fill(&mut id);
    id
}

pub(crate) fn neighbor_node_id(remote_id: &[u8], local_id: &[u8]) -> Vec<u8> {
    let mut id = Vec::with_capacity(20);
    let prefix_len = remote_id.len().min(6);
    id.extend_from_slice(&remote_id[..prefix_len]);

    if local_id.len() > prefix_len {
        id.extend_from_slice(&local_id[prefix_len..]);
    }
    while id.len() < 20 {
        id.push(rand::random());
    }
    id.truncate(20);
    id
}

#[cfg(test)]
pub(crate) fn bucket_index(id: &[u8], local_id: &[u8; 20]) -> usize {
    for bit in 0..160 {
        let byte = bit / 8;
        if byte >= id.len() {
            break;
        }
        let mask = 1 << (7 - (bit % 8));
        if (id[byte] ^ local_id[byte]) & mask != 0 {
            return bit;
        }
    }
    159
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn target_for_bucket(local_id: &[u8; 20], bucket: usize) -> [u8; 20] {
    let mut id = random_node_id();
    let bucket = bucket.min(159);
    let byte = bucket / 8;
    let bit = 7 - (bucket % 8);
    id[..byte].copy_from_slice(&local_id[..byte]);
    // Preserve the prefix before the differing bit, flip that bit, and retain
    // random bits below it, including the remainder of the same byte.
    let lower_mask = ((1u16 << bit) - 1) as u8;
    id[byte] = ((local_id[byte] & !lower_mask) ^ (1 << bit)) | (id[byte] & lower_mask);
    id
}

pub(crate) fn target_for_prefix(prefix: u8) -> [u8; 20] {
    let mut id = random_node_id();
    id[0] = prefix;
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_ids_are_eight_bytes() {
        let first = 1u64.to_be_bytes();
        assert_eq!(transaction_id_from_bytes(&first), Some(first));
        assert!(transaction_id_from_bytes(&[1, 2]).is_none());
    }

    #[test]
    fn neighbor_id_keeps_remote_prefix_and_local_suffix() {
        let remote = [1u8; 20];
        let local = [2u8; 20];
        let id = neighbor_node_id(&remote, &local);

        assert_eq!(&id[..6], &[1u8; 6]);
        assert_eq!(&id[6..], &[2u8; 14]);
    }

    #[test]
    fn bucket_targets_keep_the_requested_distance_and_randomize_same_byte_suffix() {
        let local = [0b1010_1010; 20];
        for bucket in 0..160 {
            let mut seen_one = 0u8;
            let mut seen_zero = 0u8;
            for _ in 0..128 {
                let target = target_for_bucket(&local, bucket);
                assert_eq!(bucket_index(&target, &local), bucket);
                seen_one |= target[bucket / 8];
                seen_zero |= !target[bucket / 8];
            }
            let lower_mask = ((1u16 << (7 - bucket % 8)) - 1) as u8;
            assert_eq!(seen_one & lower_mask, lower_mask);
            assert_eq!(seen_zero & lower_mask, lower_mask);
        }
    }

    #[test]
    fn prefix_targets_keep_all_remaining_bytes_random() {
        let mut seen_one = [0u8; 20];
        let mut seen_zero = [0u8; 20];
        for _ in 0..128 {
            let target = target_for_prefix(83);
            assert_eq!(target[0], 83);
            for i in 1..20 {
                seen_one[i] |= target[i];
                seen_zero[i] |= !target[i];
            }
        }
        assert_eq!(&seen_one[1..], &[u8::MAX; 19]);
        assert_eq!(&seen_zero[1..], &[u8::MAX; 19]);
    }
}
