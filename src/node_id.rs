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

pub(crate) fn target_for_bucket(local_id: &[u8; 20], bucket: usize) -> [u8; 20] {
    let mut id = *local_id;
    let bucket = bucket.min(159);
    let byte = bucket / 8;
    let bit = 7 - (bucket % 8);
    id[byte] ^= 1 << bit;
    for item in id.iter_mut().skip(byte + 1) {
        *item = rand::random();
    }
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
}
