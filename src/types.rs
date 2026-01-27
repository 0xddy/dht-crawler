use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetMode {
    Ipv4Only,
    Ipv6Only,
    #[default]
    DualStack,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorrentInfo {
    pub info_hash: String,
    pub magnet_link: String,
    pub name: String,
    pub total_size: u64,
    pub files: Vec<FileInfo>,
    pub piece_length: u64,
    pub peers: Vec<String>,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInfo {
    pub path: String,
    pub size: u64,
}

impl TorrentInfo {
    pub fn format_size(&self) -> String {
        format_bytes(self.total_size)
    }
}

impl FileInfo {
    pub fn format_size(&self) -> String {
        format_bytes(self.size)
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_index = 0;

    while size >= 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }

    format!("{:.2} {}", size, UNITS[unit_index])
}

#[derive(Debug, Clone)]
pub struct DHTOptions {
    pub port: u16,

    pub auto_metadata: bool,

    pub metadata_timeout: u64,

    pub max_metadata_queue_size: usize,

    pub max_metadata_worker_count: usize,

    pub netmode: NetMode,

    pub node_queue_capacity: usize,

    pub hash_queue_capacity: usize,
}

impl Default for DHTOptions {
    fn default() -> Self {
        Self {
            port: 6881,
            auto_metadata: true,
            metadata_timeout: 3,
            max_metadata_queue_size: 100000,
            max_metadata_worker_count: 1000,
            netmode: NetMode::Ipv4Only,
            node_queue_capacity: 100000,
            hash_queue_capacity: 10000,
        }
    }
}
