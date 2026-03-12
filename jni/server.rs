use crate::{DHTServer, DHTOptions};
use std::sync::Arc;
use tokio::runtime::Runtime;

/// JNI 侧持有的服务器句柄，包含 tokio runtime 和 DHTServer 实例。
/// 通过 `Box::into_raw` 转成 `jlong` 句柄传给 Java，
/// 在 destroy 时通过 `Box::from_raw` 恢复并 drop。
pub struct ServerHandle {
    pub runtime: Runtime,
    pub server: Arc<DHTServer>,
}

impl ServerHandle {
    /// 在新建的 tokio runtime 里初始化 DHTServer。
    pub fn new(options: DHTOptions) -> Result<Self, String> {
        let runtime = Runtime::new().map_err(|e| format!("无法创建 tokio runtime: {e}"))?;
        let server = runtime
            .block_on(DHTServer::new(options))
            .map_err(|e| format!("DHTServer 初始化失败: {e}"))?;
        Ok(Self {
            runtime,
            server: Arc::new(server),
        })
    }

    /// 在 runtime 里 spawn server.start()，不阻塞调用线程。
    pub fn start(&self) -> Result<(), String> {
        let server: Arc<DHTServer> = Arc::clone(&self.server);
        self.runtime.spawn(async move {
            if let Err(e) = server.start().await {
                log::error!("DHT server 运行错误: {e}");
            }
        });
        Ok(())
    }

    /// 发送关闭信号（非阻塞）。
    pub fn stop(&self) {
        self.server.shutdown();
    }

    /// 返回节点池大小。
    pub fn node_pool_size(&self) -> usize {
        self.server.get_node_pool_size()
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// 句柄指针工具
// ──────────────────────────────────────────────────────────────────────────────

/// 将 `ServerHandle` 装箱并返回原始指针，供 Java 以 `long` 持有。
pub fn into_handle_ptr(handle: ServerHandle) -> i64 {
    Box::into_raw(Box::new(handle)) as i64
}

/// 从 Java 传入的 `long` 句柄获取不可变引用。
///
/// # Safety
/// 调用方必须确保句柄未被 destroy，且在单次 JNI 调用生命周期内使用。
pub unsafe fn handle_ref<'a>(ptr: i64) -> Option<&'a ServerHandle> {
    if ptr == 0 {
        return None;
    }
    Some(unsafe { &*(ptr as *const ServerHandle) })
}

/// 消费句柄：从裸指针重建 Box 并 drop，释放所有资源（包括 runtime）。
///
/// # Safety
/// 只能调用一次，调用后 Java 侧不得再使用该句柄。
pub unsafe fn destroy_handle(ptr: i64) {
    if ptr != 0 {
        drop(unsafe { Box::from_raw(ptr as *mut ServerHandle) });
    }
}
