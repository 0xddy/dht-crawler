use serde::Deserialize;

#[derive(Deserialize, Debug)]
#[allow(dead_code)]
pub struct DhtMessage {
    pub t: serde_bytes::ByteBuf,
    #[allow(dead_code)]
    pub y: String,
    #[allow(dead_code)]
    pub q: Option<String>,
    pub a: Option<DhtArgs>,
    pub r: Option<DhtResponse>,
}

#[derive(Deserialize, Debug)]
pub struct DhtArgs {
    pub id: Option<serde_bytes::ByteBuf>,
    pub target: Option<serde_bytes::ByteBuf>,
    pub info_hash: Option<serde_bytes::ByteBuf>,
    pub token: Option<serde_bytes::ByteBuf>,
    pub port: Option<u16>,
    pub implied_port: Option<u8>,
}

#[derive(Deserialize, Debug)]
pub struct DhtResponse {
    #[serde(default)]
    #[allow(dead_code)]
    pub id: Option<serde_bytes::ByteBuf>,
    #[serde(default)]
    pub nodes: Option<serde_bytes::ByteBuf>,
    #[serde(default)]
    pub nodes6: Option<serde_bytes::ByteBuf>,
}

