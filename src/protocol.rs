use serde::Deserialize;

#[derive(Deserialize, Debug, Clone)]
#[allow(dead_code)]
/// Decoded KRPC envelope.
pub struct DhtMessage {
    /// Transaction ID bytes.
    pub t: serde_bytes::ByteBuf,
    #[allow(dead_code)]
    /// Message kind (`q`, `r`, or `e`).
    pub y: String,
    #[allow(dead_code)]
    /// Query method when `y == q`.
    pub q: Option<String>,
    /// Query arguments.
    pub a: Option<DhtArgs>,
    /// Response dictionary.
    pub r: Option<DhtResponse>,
}

#[derive(Deserialize, Debug, Clone)]
/// Supported BEP-5 query arguments.
pub struct DhtArgs {
    /// Sender node ID.
    pub id: Option<serde_bytes::ByteBuf>,
    /// find_node target ID.
    pub target: Option<serde_bytes::ByteBuf>,
    /// get_peers/announce InfoHash.
    pub info_hash: Option<serde_bytes::ByteBuf>,
    /// announce validation token.
    pub token: Option<serde_bytes::ByteBuf>,
    /// Explicit announced Peer port.
    pub port: Option<u16>,
    /// Non-zero means use the UDP source port.
    pub implied_port: Option<u8>,
}

#[derive(Deserialize, Debug, Clone)]
/// Supported BEP-5 response fields.
pub struct DhtResponse {
    #[serde(default)]
    #[allow(dead_code)]
    /// Responder node ID.
    pub id: Option<serde_bytes::ByteBuf>,
    #[serde(default)]
    /// Compact IPv4 node tuples.
    pub nodes: Option<serde_bytes::ByteBuf>,
    #[serde(default)]
    /// Compact IPv6 node tuples.
    pub nodes6: Option<serde_bytes::ByteBuf>,
    #[serde(default)]
    /// Compact Peer endpoints returned by `get_peers`.
    pub values: Option<Vec<serde_bytes::ByteBuf>>,
}
