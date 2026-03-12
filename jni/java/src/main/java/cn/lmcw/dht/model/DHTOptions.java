package cn.lmcw.dht.model;

/**
 * 与 Rust {@code DHTOptions} 一一对应的配置对象。
 * <p>通过 JNI 传入 Rust 侧，由 Rust 读取各字段构造 {@code DHTOptions}。</p>
 *
 * <h3>netMode 取值</h3>
 * <ul>
 *   <li>0 - IPv4 Only</li>
 *   <li>1 - IPv6 Only</li>
 *   <li>2 - Dual Stack（默认）</li>
 * </ul>
 */
public final class DHTOptions {

    /** 监听端口，默认 6881 */
    private int port = 6881;

    /** 获取 metadata 超时（秒），默认 3 */
    private long metadataTimeout = 3L;

    /** metadata 队列最大容量，默认 100000 */
    private int maxMetadataQueueSize = 100_000;

    /** 并发 metadata 拉取 worker 数量，默认 1000 */
    private int maxMetadataWorkerCount = 1_000;

    /** 节点队列容量，默认 100000 */
    private int nodeQueueCapacity = 100_000;

    /** hash 队列容量，默认 10000 */
    private int hashQueueCapacity = 10_000;

    /**
     * 网络模式：0=IPv4Only, 1=IPv6Only, 2=DualStack（默认 0 / IPv4Only，
     * 与 Rust 侧 {@code NetMode::Ipv4Only} 对应）
     */
    private int netMode = 0;

    public DHTOptions() {}

    public int getPort() { return port; }
    public DHTOptions setPort(int port) { this.port = port; return this; }

    public long getMetadataTimeout() { return metadataTimeout; }
    public DHTOptions setMetadataTimeout(long metadataTimeout) {
        this.metadataTimeout = metadataTimeout;
        return this;
    }

    public int getMaxMetadataQueueSize() { return maxMetadataQueueSize; }
    public DHTOptions setMaxMetadataQueueSize(int maxMetadataQueueSize) {
        this.maxMetadataQueueSize = maxMetadataQueueSize;
        return this;
    }

    public int getMaxMetadataWorkerCount() { return maxMetadataWorkerCount; }
    public DHTOptions setMaxMetadataWorkerCount(int maxMetadataWorkerCount) {
        this.maxMetadataWorkerCount = maxMetadataWorkerCount;
        return this;
    }

    public int getNodeQueueCapacity() { return nodeQueueCapacity; }
    public DHTOptions setNodeQueueCapacity(int nodeQueueCapacity) {
        this.nodeQueueCapacity = nodeQueueCapacity;
        return this;
    }

    public int getHashQueueCapacity() { return hashQueueCapacity; }
    public DHTOptions setHashQueueCapacity(int hashQueueCapacity) {
        this.hashQueueCapacity = hashQueueCapacity;
        return this;
    }

    public int getNetMode() { return netMode; }
    public DHTOptions setNetMode(int netMode) { this.netMode = netMode; return this; }

    @Override
    public String toString() {
        return "DHTOptions{"
                + "port=" + port
                + ", metadataTimeout=" + metadataTimeout
                + ", netMode=" + netMode
                + '}';
    }
}
