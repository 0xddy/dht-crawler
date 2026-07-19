//! UDP 收包缓冲区池：避免每包 `to_owned()` 拷贝。
//!
//! 单线程 listener 从池中取出固定大小缓冲区，`recv_from` 直接写入；
//! 通过 channel 将缓冲区所有权交给 worker，处理完毕后归还池中复用。

use crossbeam_queue::ArrayQueue;
use std::sync::Arc;

/// 与 `process_udp_packet` 中丢弃阈值一致
pub const MAX_DHT_UDP_PACKET: usize = 8192;

/// 预分配缓冲区数量（约等于高峰在途包数）
const INITIAL_POOL_SIZE: usize = 512;
/// 池上限，防止极端背压下无限增长
const MAX_POOL_SIZE: usize = 4096;

/// 在途 UDP 包：固定容量缓冲区 + 有效长度
pub struct UdpPacket {
    pub buf: Box<[u8]>,
    pub len: usize,
}

impl UdpPacket {
    #[inline]
    pub fn payload(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

/// 固定 8KiB 缓冲区的对象池（`recv_from` 零拷贝移交 worker）
#[derive(Clone)]
pub struct UdpBufferPool {
    inner: Arc<PoolInner>,
}

struct PoolInner {
    free: ArrayQueue<Box<[u8]>>,
    buf_capacity: usize,
}

impl UdpBufferPool {
    pub fn new() -> Self {
        let free = ArrayQueue::new(MAX_POOL_SIZE);
        for _ in 0..INITIAL_POOL_SIZE {
            let _ = free.push(alloc_buffer(MAX_DHT_UDP_PACKET));
        }
        Self {
            inner: Arc::new(PoolInner {
                free,
                buf_capacity: MAX_DHT_UDP_PACKET,
            }),
        }
    }

    /// 取一块缓冲区；池空时分配新块（背压或突发流量）
    pub fn acquire(&self) -> Box<[u8]> {
        self.inner
            .free
            .pop()
            .unwrap_or_else(|| alloc_buffer(self.inner.buf_capacity))
    }

    /// 归还缓冲区；池满时直接丢弃，由 GC 回收
    pub fn release(&self, buf: Box<[u8]>) {
        if buf.len() != self.inner.buf_capacity {
            return;
        }
        let _ = self.inner.free.push(buf);
    }

    pub fn buf_capacity(&self) -> usize {
        self.inner.buf_capacity
    }
}

fn alloc_buffer(capacity: usize) -> Box<[u8]> {
    let v = vec![0; capacity];
    v.into_boxed_slice()
}
