use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;
use bytes::Bytes;
#[cfg(feature = "metrics")]
use metrics::{counter, histogram};
use sha1::{Digest, Sha1};
use tokio::time::timeout;
use rbit::{
    metadata_piece_count, ExtensionHandshake, Message, MetadataMessage,
    MetadataMessageType, PeerConnection, PeerId,
};
use rbit::peer::ExtensionMessage;
use crate::types::FileInfo;

#[derive(Clone)]
pub struct RbitFetcher {
    timeout: Duration,
}

impl RbitFetcher {
    pub fn new(timeout_secs: u64) -> Self {
        Self {
            timeout: Duration::from_secs(if timeout_secs == 0 { 15 } else { timeout_secs }),
        }
    }

    pub async fn fetch(
        &self,
        info_hash: &[u8; 20],
        peer_addr: SocketAddr,
    ) -> Option<(String, u64, Vec<FileInfo>)> {
        #[cfg(feature = "metrics")]
        counter!("dht_metadata_fetch_attempts_total").increment(1);

        let peer_id = PeerId::generate();

        let mut conn = match timeout(
            Duration::from_secs(3),
            PeerConnection::connect(peer_addr, *info_hash, *peer_id.as_bytes()),
        ).await {
            Ok(Ok(c)) => {
                #[cfg(feature = "metrics")]
                counter!("dht_metadata_connection_result_total", "result" => "success").increment(1);
                c
            },
            Ok(Err(_)) => {
                #[cfg(feature = "metrics")]
                counter!("dht_metadata_connection_result_total", "result" => "failed").increment(1);
                return None;
            },
            Err(_) => {
                #[cfg(feature = "metrics")]
                counter!("dht_metadata_connection_result_total", "result" => "timeout").increment(1);
                return None;
            },
        };

        if !conn.supports_extension {
            #[cfg(feature = "metrics")]
            counter!("dht_metadata_handshake_result_total", "result" => "no_extension_support").increment(1);
            return None;
        }

        let my_ut_metadata_id = 1;
        let handshake = ExtensionHandshake::with_extensions(&[("ut_metadata", my_ut_metadata_id)]);

        if let Ok(handshake_bytes) = handshake.encode() {
            let _ = conn.send(Message::Extended { id: 0, payload: handshake_bytes }).await;
        } else {
            return None;
        }

        let mut metadata_size = 0;
        let mut remote_ut_metadata_id = 0;
        let mut pieces: BTreeMap<u32, Bytes> = BTreeMap::new();
        let mut request_sent = false;

        let result = timeout(self.timeout, async {
            loop {
                let msg = conn.receive().await.ok()?;
                match msg {
                    Message::Extended { id, payload } => {
                        if id == 0 {
                            if let Ok(ExtensionMessage::Handshake(remote_hs)) = ExtensionMessage::decode(id, &payload) {
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
                                    counter!("dht_metadata_fetch_fail_total", "reason" => "size_limit").increment(1);
                                    return None; 
                                }

                                let count = metadata_piece_count(metadata_size as usize);
                                for i in 0..count {
                                    let req = MetadataMessage::request(i as u32);
                                    if let Ok(encoded) = req.encode() {
                                        let _ = conn.send(Message::Extended { id: remote_ut_metadata_id, payload: encoded }).await;
                                    }
                                }
                                request_sent = true;
                            }
                        } else if id == my_ut_metadata_id {
                            if let Ok(meta_msg) = MetadataMessage::decode(&payload) {
                                if meta_msg.msg_type == MetadataMessageType::Data {
                                    if let Some(data) = meta_msg.data {
                                        #[cfg(feature = "metrics")]
                                        counter!("dht_metadata_bytes_downloaded_total").increment(data.len() as u64);
                                        pieces.insert(meta_msg.piece, data);
                                    }
                                }
                            }
                            if metadata_size > 0 {
                                let total_received: usize = pieces.values().map(|p| p.len()).sum();
                                if total_received >= metadata_size as usize {
                                    let mut full_data = Vec::with_capacity(metadata_size as usize);
                                    let count = metadata_piece_count(metadata_size as usize);
                                    let mut success = true;
                                    for i in 0..count {
                                        if let Some(p) = pieces.get(&(i as u32)) {
                                            full_data.extend_from_slice(p);
                                        } else {
                                            success = false; break;
                                        }
                                    }
                                    if success {
                                        let info_hash_copy = *info_hash;
                                        let full_data_clone = full_data.clone();
                                        let is_valid = tokio::task::spawn_blocking(move || {
                                            let mut hasher = Sha1::new();
                                            hasher.update(&full_data_clone);
                                            let digest: [u8; 20] = hasher.finalize().into();
                                            digest == info_hash_copy
                                        }).await.unwrap_or(false);

                                        if is_valid {
                                            #[cfg(feature = "metrics")]
                                            counter!("dht_metadata_handshake_result_total", "result" => "success").increment(1);
                                            return Some(full_data);
                                        }
                                        #[cfg(feature = "metrics")]
                                        counter!("dht_metadata_fetch_fail_total", "reason" => "sha1_mismatch").increment(1);
                                        return None;
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }).await;

        match result {
            Ok(Some(info_bytes)) => {
                if let Ok(value) = rbit::decode(&info_bytes) {
                    if let Some(dict) = value.as_dict() {
                        let name = dict.get(&b"name"[..]).and_then(|v| v.as_str()).unwrap_or("Unknown").to_string();
                        let mut total_size = 0;
                        let mut file_list = Vec::new();
                        if let Some(files) = dict.get(&b"files"[..]).and_then(|v| v.as_list()) {
                            for file in files {
                                if let Some(f_dict) = file.as_dict() {
                                    if let Some(len) = f_dict.get(&b"length"[..]).and_then(|v| v.as_integer()) {
                                        let len = len as u64;
                                        total_size += len;
                                        let mut path_parts = Vec::new();
                                        if let Some(path_list) = f_dict.get(&b"path"[..]).and_then(|v| v.as_list()) {
                                            for p in path_list {
                                                if let Some(p_str) = p.as_str() { path_parts.push(p_str); }
                                            }
                                        }
                                        file_list.push(FileInfo { path: path_parts.join("/"), size: len });
                                    }
                                }
                            }
                        } else if let Some(len) = dict.get(&b"length"[..]).and_then(|v| v.as_integer()) {
                            total_size = len as u64;
                            file_list.push(FileInfo { path: name.clone(), size: total_size });
                        }
                        if total_size > 0 { 
                            #[cfg(feature = "metrics")]
                            {
                                counter!("dht_metadata_fetch_success_total").increment(1);
                                histogram!("dht_metadata_size_bytes").record(total_size as f64);
                            }
                            return Some((name, total_size, file_list)); 
                        }
                    }
                }
                #[cfg(feature = "metrics")]
                counter!("dht_metadata_fetch_fail_total", "reason" => "parse_error").increment(1);
                None
            }
            _ => {
                #[cfg(feature = "metrics")]
                counter!("dht_metadata_fetch_fail_total", "reason" => "timeout").increment(1);
                None
            },
        }
    }
}