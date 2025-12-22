mod error;
mod server;
pub mod protocol;
pub mod types;
pub mod metadata;  // 公开 metadata 模块
mod sharded;  // 分片锁模块
pub mod scheduler;  // 元数据调度器

pub use error::{DHTError, Result};
pub use server::{DHTServer, HashDiscovered};
pub use types::{DHTOptions, FileInfo, TorrentInfo};
pub use sharded::{ShardedBloom, ShardedNodeQueue, NodeTuple};
pub use scheduler::MetadataScheduler;

// 重新导出常用类型
pub mod prelude {
    pub use crate::error::{DHTError, Result};
    pub use crate::server::DHTServer;
    pub use crate::types::{DHTOptions, FileInfo, TorrentInfo};
    pub use crate::scheduler::MetadataScheduler;
}
