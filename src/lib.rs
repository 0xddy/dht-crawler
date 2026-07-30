//! High-throughput BitTorrent DHT crawler with bounded crawl and Metadata pipelines.
//!
//! [`DHTServer`] is the primary entry point. Configure it with [`DHTOptions`], register
//! callbacks, then await [`DHTServer::start`] until another task calls [`DHTServer::shutdown`].
//! Runtime counters are available through [`DHTServer::runtime_stats`] without enabling any
//! exporter. See the repository README for scheduling, backpressure and migration details.

mod addr;
mod bootstrap;
mod budget;
mod crawl_config;
mod crawl_engine;
mod error;
mod krpc;
/// BEP-9 Metadata fetch support.
pub mod metadata;
mod node_id;
mod node_pool;
mod peer_lookup;
/// Serializable BEP-5 KRPC wire types.
pub mod protocol;
mod routing_snapshot;
mod runtime_stats;
/// Bounded, deduplicating Metadata scheduler.
pub mod scheduler;
mod server;
/// Public configuration, callback payload and network types.
pub mod types;
mod udp_buffer;
mod udp_ingress;

pub use error::{DHTError, Result};
pub use runtime_stats::{
    DhtObservabilitySnapshot, DhtRuntimeSnapshot, DhtRuntimeStats, FixedHistogramSnapshot,
};
pub use scheduler::{MetadataScheduler, MetadataSchedulerCallbacks, MetadataSchedulerLimits};
pub use server::{DHTServer, HashDiscovered};
pub use types::{
    BootstrapOptions, CrawlOptions, DHTOptions, FileInfo, MetadataFetchCompletion,
    MetadataFetchCompletionStatus, MetadataOptions, NetMode, NodeTuple, PeerLookupOptions,
    PoolOptions, RateLimitOptions, SchedulerOptions, TargetOptions, TorrentInfo,
};

/// Common server, configuration and callback payload imports.
pub mod prelude {
    pub use crate::error::{DHTError, Result};
    pub use crate::runtime_stats::{DhtRuntimeSnapshot, DhtRuntimeStats};
    pub use crate::scheduler::{
        MetadataScheduler, MetadataSchedulerCallbacks, MetadataSchedulerLimits,
    };
    pub use crate::server::DHTServer;
    pub use crate::types::{
        BootstrapOptions, CrawlOptions, DHTOptions, FileInfo, MetadataFetchCompletion,
        MetadataFetchCompletionStatus, MetadataOptions, NetMode, NodeTuple, PeerLookupOptions,
        PoolOptions, RateLimitOptions, SchedulerOptions, TargetOptions, TorrentInfo,
    };
}

#[cfg(feature = "jni")]
#[path = "../jni/mod.rs"]
/// JNI entry points consumed by the external JVM bindings.
pub mod jni_bindings;
