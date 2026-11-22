use super::bencode::{self, BencodeValue};
use bytes::Bytes;
use rand::Rng;
use std::collections::BTreeMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

pub(crate) const PROTOCOL_HEADER: &[u8; 20] = b"\x13BitTorrent protocol";
pub(crate) const HANDSHAKE_LEN: usize = 68;
const EXT_FLAG_BYTE: usize = 5;
const EXT_FLAG_MASK: u8 = 0x10;
const MSG_ID_EXTENDED: u8 = 20;
pub(crate) const OUR_UT_METADATA_ID: u8 = 1;
const OUR_EXT_HANDSHAKE_BYTES: &[u8] = b"d1:md11:ut_metadatai1eee";
pub(crate) const MAX_WIRE_MESSAGE_LEN: usize = 2 * 1024 * 1024;
const READ_BUF_CAPACITY: usize = 8 * 1024;

#[derive(Debug)]
pub(crate) enum PeerWireError {
    Io(std::io::Error),
    InvalidHandshake,
    InfoHashMismatch,
    MessageTooLarge(#[allow(dead_code)] usize),
    InvalidFrame,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Handshake {
    pub reserved: [u8; 8],
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
}

impl Handshake {
    pub(crate) fn new(info_hash: [u8; 20], peer_id: [u8; 20]) -> Self {
        let mut reserved = [0u8; 8];
        reserved[EXT_FLAG_BYTE] |= EXT_FLAG_MASK;
        Self {
            reserved,
            info_hash,
            peer_id,
        }
    }

    pub(crate) fn supports_extension_protocol(&self) -> bool {
        (self.reserved[EXT_FLAG_BYTE] & EXT_FLAG_MASK) != 0
    }

    pub(crate) fn encode(&self) -> [u8; HANDSHAKE_LEN] {
        let mut buf = [0u8; HANDSHAKE_LEN];
        buf[0..20].copy_from_slice(PROTOCOL_HEADER);
        buf[20..28].copy_from_slice(&self.reserved);
        buf[28..48].copy_from_slice(&self.info_hash);
        buf[48..68].copy_from_slice(&self.peer_id);
        buf
    }

    pub(crate) fn decode(buf: &[u8; HANDSHAKE_LEN]) -> Result<Self, PeerWireError> {
        if &buf[0..20] != PROTOCOL_HEADER {
            return Err(PeerWireError::InvalidHandshake);
        }
        let mut reserved = [0u8; 8];
        let mut info_hash = [0u8; 20];
        let mut peer_id = [0u8; 20];
        reserved.copy_from_slice(&buf[20..28]);
        info_hash.copy_from_slice(&buf[28..48]);
        peer_id.copy_from_slice(&buf[48..68]);
        Ok(Self {
            reserved,
            info_hash,
            peer_id,
        })
    }
}

pub(crate) fn random_peer_id() -> [u8; 20] {
    let mut id = [0u8; 20];
    id[..8].copy_from_slice(b"-UT3550-");
    rand::thread_rng().fill(&mut id[8..]);
    id
}

pub(crate) struct PeerTransport {
    reader: BufReader<TcpStream>,
}

impl PeerTransport {
    pub(crate) fn new(stream: TcpStream) -> Self {
        Self {
            reader: BufReader::with_capacity(READ_BUF_CAPACITY, stream),
        }
    }

    pub(crate) async fn send_handshake(&mut self, hs: &Handshake) -> Result<(), PeerWireError> {
        let encoded = hs.encode();
        let stream = self.reader.get_mut();
        stream.write_all(&encoded).await.map_err(PeerWireError::Io)?;
        stream.flush().await.map_err(PeerWireError::Io)?;
        Ok(())
    }

    pub(crate) async fn receive_handshake(&mut self) -> Result<Handshake, PeerWireError> {
        let mut buf = [0u8; HANDSHAKE_LEN];
        self.reader
            .read_exact(&mut buf)
            .await
            .map_err(PeerWireError::Io)?;
        Handshake::decode(&buf)
    }

    pub(crate) async fn send_our_extension_handshake(&mut self) -> Result<(), PeerWireError> {
        self.send_extended(0, OUR_EXT_HANDSHAKE_BYTES).await
    }

    pub(crate) async fn send_extended(
        &mut self,
        ext_id: u8,
        payload: &[u8],
    ) -> Result<(), PeerWireError> {
        let msg_len = 2usize
            .checked_add(payload.len())
            .ok_or(PeerWireError::InvalidFrame)?;
        let msg_len_u32 = u32::try_from(msg_len).map_err(|_| PeerWireError::InvalidFrame)?;
        let mut frame = Vec::with_capacity(6 + payload.len());
        frame.extend_from_slice(&msg_len_u32.to_be_bytes());
        frame.push(MSG_ID_EXTENDED);
        frame.push(ext_id);
        frame.extend_from_slice(payload);

        let stream = self.reader.get_mut();
        stream.write_all(&frame).await.map_err(PeerWireError::Io)?;
        stream.flush().await.map_err(PeerWireError::Io)?;
        Ok(())
    }

    pub(crate) async fn receive_next_extended(&mut self) -> Result<(u8, Bytes), PeerWireError> {
        let mut len_buf = [0u8; 4];
        let mut scratch = [0u8; 4096];
        loop {
            self.reader
                .read_exact(&mut len_buf)
                .await
                .map_err(PeerWireError::Io)?;
            let len = u32::from_be_bytes(len_buf) as usize;
            if len == 0 {
                continue;
            }
            if len > MAX_WIRE_MESSAGE_LEN {
                return Err(PeerWireError::MessageTooLarge(len));
            }
            let msg_id = self.reader.read_u8().await.map_err(PeerWireError::Io)?;
            if msg_id != MSG_ID_EXTENDED {
                let mut remaining = len - 1;
                while remaining > 0 {
                    let chunk = remaining.min(scratch.len());
                    self.reader
                        .read_exact(&mut scratch[..chunk])
                        .await
                        .map_err(PeerWireError::Io)?;
                    remaining -= chunk;
                }
                continue;
            }
            if len < 2 {
                return Err(PeerWireError::InvalidFrame);
            }
            let ext_id = self.reader.read_u8().await.map_err(PeerWireError::Io)?;
            let payload_len = len - 2;
            let mut payload = vec![0u8; payload_len];
            self.reader
                .read_exact(&mut payload)
                .await
                .map_err(PeerWireError::Io)?;
            return Ok((ext_id, Bytes::from(payload)));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RemoteExtensionHandshake {
    pub ut_metadata_id: Option<u8>,
    pub metadata_size: Option<i64>,
}

impl RemoteExtensionHandshake {
    pub(crate) fn decode(payload: &[u8]) -> Result<Self, PeerWireError> {
        let (val, _) = bencode::decode_prefix(payload).map_err(|_| PeerWireError::InvalidFrame)?;
        let dict = val.as_dict().ok_or(PeerWireError::InvalidFrame)?;
        let ut_metadata_id = dict
            .get(&b"m"[..])
            .and_then(|m| m.as_dict())
            .and_then(|m| m.get(&b"ut_metadata"[..]))
            .and_then(|v| v.as_integer())
            .filter(|&id| id > 0)
            .and_then(|id| u8::try_from(id).ok());
        let metadata_size = dict
            .get(&b"metadata_size"[..])
            .and_then(|v| v.as_integer());
        Ok(Self {
            ut_metadata_id,
            metadata_size,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MetadataMsg {
    Request { piece: u32 },
    Data { piece: u32, data: Bytes },
    Reject { piece: u32 },
}

impl MetadataMsg {
    pub(crate) fn encode_request(piece: u32) -> Vec<u8> {
        let mut dict = BTreeMap::new();
        dict.insert(
            Bytes::from_static(b"msg_type"),
            BencodeValue::Integer(0),
        );
        dict.insert(
            Bytes::from_static(b"piece"),
            BencodeValue::Integer(i64::from(piece)),
        );
        bencode::encode(&BencodeValue::Dict(dict))
    }

    pub(crate) fn decode(payload: Bytes) -> Result<Self, PeerWireError> {
        let (val, consumed) =
            bencode::decode_prefix(&payload).map_err(|_| PeerWireError::InvalidFrame)?;
        let dict = val.as_dict().ok_or(PeerWireError::InvalidFrame)?;
        let msg_type = dict
            .get(&b"msg_type"[..])
            .and_then(|v| v.as_integer())
            .ok_or(PeerWireError::InvalidFrame)?;
        let piece_i64 = dict
            .get(&b"piece"[..])
            .and_then(|v| v.as_integer())
            .ok_or(PeerWireError::InvalidFrame)?;
        let piece = u32::try_from(piece_i64).map_err(|_| PeerWireError::InvalidFrame)?;
        match msg_type {
            0 => Ok(Self::Request { piece }),
            1 => Ok(Self::Data {
                piece,
                data: payload.slice(consumed..),
            }),
            2 => Ok(Self::Reject { piece }),
            _ => Err(PeerWireError::InvalidFrame),
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) struct TestPeerConn {
        transport: PeerTransport,
    }

    impl TestPeerConn {
        pub(crate) async fn accept(
            stream: TcpStream,
            info_hash: [u8; 20],
            peer_id: [u8; 20],
        ) -> Result<Self, PeerWireError> {
            let mut transport = PeerTransport::new(stream);
            let client = transport.receive_handshake().await?;
            if client.info_hash != info_hash {
                return Err(PeerWireError::InfoHashMismatch);
            }
            transport
                .send_handshake(&Handshake::new(info_hash, peer_id))
                .await?;
            Ok(Self { transport })
        }

        pub(crate) async fn receive_extended(&mut self) -> Result<(u8, Bytes), PeerWireError> {
            self.transport.receive_next_extended().await
        }

        pub(crate) async fn send_extended(
            &mut self,
            ext_id: u8,
            payload: &[u8],
        ) -> Result<(), PeerWireError> {
            self.transport.send_extended(ext_id, payload).await
        }
    }

    pub(crate) fn encode_ext_handshake(
        ut_metadata_id: Option<u8>,
        metadata_size: Option<i64>,
    ) -> Vec<u8> {
        let mut m = BTreeMap::new();
        if let Some(id) = ut_metadata_id {
            m.insert(
                Bytes::from_static(b"ut_metadata"),
                BencodeValue::Integer(i64::from(id)),
            );
        }
        let mut root = BTreeMap::new();
        root.insert(Bytes::from_static(b"m"), BencodeValue::Dict(m));
        if let Some(size) = metadata_size {
            root.insert(
                Bytes::from_static(b"metadata_size"),
                BencodeValue::Integer(size),
            );
        }
        bencode::encode(&BencodeValue::Dict(root))
    }

    pub(crate) fn encode_metadata_data(piece: u32, total_size: u64, data: &[u8]) -> Vec<u8> {
        let mut root = BTreeMap::new();
        root.insert(Bytes::from_static(b"msg_type"), BencodeValue::Integer(1));
        root.insert(
            Bytes::from_static(b"piece"),
            BencodeValue::Integer(i64::from(piece)),
        );
        root.insert(
            Bytes::from_static(b"total_size"),
            BencodeValue::Integer(total_size as i64),
        );
        let mut out = bencode::encode(&BencodeValue::Dict(root));
        out.extend_from_slice(data);
        out
    }

    pub(crate) fn encode_metadata_reject(piece: u32) -> Vec<u8> {
        let mut root = BTreeMap::new();
        root.insert(Bytes::from_static(b"msg_type"), BencodeValue::Integer(2));
        root.insert(
            Bytes::from_static(b"piece"),
            BencodeValue::Integer(i64::from(piece)),
        );
        bencode::encode(&BencodeValue::Dict(root))
    }
}
