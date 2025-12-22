use serde::{Deserialize, Serialize};

/// 完整的种子信息（包含元数据）
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

/// 文件信息
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

/// DHT 服务器配置
#[derive(Debug, Clone)]
pub struct DHTOptions {
    /// DHT 端口
    pub port: u16,

    /// 是否自动获取元数据
    pub auto_metadata: bool,

    /// 元数据获取超时（秒）
    pub metadata_timeout: u64,

    /// 元数据获取队列大小（背压限制）
    pub max_metadata_queue_size: usize,

    /// 并发元数据获取工作线程数
    pub max_metadata_worker_count: usize,
}

impl Default for DHTOptions {
    fn default() -> Self {
        Self {
            port: 0,
            auto_metadata: true,
            // 缩短超时，快速失败，不等待慢节点
            metadata_timeout: 10,
            // 加大队列，防止流量高峰丢包
            max_metadata_queue_size: 10000,
            // 提高并发，模拟 Node.js 的高并发 IO
            max_metadata_worker_count: 1000,
        }
    }
}