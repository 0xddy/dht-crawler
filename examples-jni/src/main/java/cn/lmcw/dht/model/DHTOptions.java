package cn.lmcw.dht.model;

/**
 * Rust {@code DHTOptions} 的 JNI 扁平化子集。
 * <p>通过 JNI 传入 Rust 侧；未暴露的 Metadata failure cache、Bootstrap、Target 和
 * Scheduler 字段采用 Rust 0.2 默认值。Java 与 native 库版本必须保持一致。</p>
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
    private long metadataTimeout = 4L;

    /** metadata 队列最大容量，默认 100000 */
    private int maxMetadataQueueSize = 10_000;

    /** 并发 metadata 拉取 worker 数量，默认 1000 */
    private int maxMetadataWorkerCount = 256;

    /** 节点队列容量，默认 100000 */
    private int poolCapacity = 100_000;

    private int findNodeRatePerSecond = 200;

    private int findNodeBurst = 40;

    private int maxFindNodeInFlight = 512;

    private int maxNewDestinationsPerMinute = 10_000;

    private int maxReplacementsPerMinute = 25_000;

    private long requestTimeoutSeconds = 2L;
    private int maxResponseRatePerSecond = 500;
    private long maxResponseBytesPerSecond = 1_048_576L;
    private int maxResponseRatePerSource = 40;
    private int metadataPressureFloorPercent = 25;
    private long recentProbeTtlSeconds = 600L;
    private int responsiveCapacity = 16_384;
    private long responsiveTtlSeconds = 900L;
    private int poolLowWatermark = 10_000;
    private int maxInFlightPerSubnet = 8;

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

    public int getPoolCapacity() { return poolCapacity; }
    public DHTOptions setPoolCapacity(int poolCapacity) {
        this.poolCapacity = poolCapacity;
        return this;
    }

    public int getFindNodeRatePerSecond() { return findNodeRatePerSecond; }
    public DHTOptions setFindNodeRatePerSecond(int value) { this.findNodeRatePerSecond = value; return this; }

    public int getFindNodeBurst() { return findNodeBurst; }
    public DHTOptions setFindNodeBurst(int value) { this.findNodeBurst = value; return this; }

    public int getMaxFindNodeInFlight() { return maxFindNodeInFlight; }
    public DHTOptions setMaxFindNodeInFlight(int value) { this.maxFindNodeInFlight = value; return this; }

    public int getMaxNewDestinationsPerMinute() { return maxNewDestinationsPerMinute; }
    public DHTOptions setMaxNewDestinationsPerMinute(int value) { this.maxNewDestinationsPerMinute = value; return this; }

    public int getMaxReplacementsPerMinute() { return maxReplacementsPerMinute; }
    public DHTOptions setMaxReplacementsPerMinute(int value) { this.maxReplacementsPerMinute = value; return this; }

    public long getRequestTimeoutSeconds() { return requestTimeoutSeconds; }
    public DHTOptions setRequestTimeoutSeconds(long value) { this.requestTimeoutSeconds = value; return this; }
    public int getMaxResponseRatePerSecond() { return maxResponseRatePerSecond; }
    public DHTOptions setMaxResponseRatePerSecond(int value) { this.maxResponseRatePerSecond = value; return this; }
    public long getMaxResponseBytesPerSecond() { return maxResponseBytesPerSecond; }
    public DHTOptions setMaxResponseBytesPerSecond(long value) { this.maxResponseBytesPerSecond = value; return this; }
    public int getMaxResponseRatePerSource() { return maxResponseRatePerSource; }
    public DHTOptions setMaxResponseRatePerSource(int value) { this.maxResponseRatePerSource = value; return this; }
    public int getMetadataPressureFloorPercent() { return metadataPressureFloorPercent; }
    public DHTOptions setMetadataPressureFloorPercent(int value) { this.metadataPressureFloorPercent = value; return this; }
    public long getRecentProbeTtlSeconds() { return recentProbeTtlSeconds; }
    public DHTOptions setRecentProbeTtlSeconds(long value) { this.recentProbeTtlSeconds = value; return this; }
    public int getResponsiveCapacity() { return responsiveCapacity; }
    public DHTOptions setResponsiveCapacity(int value) { this.responsiveCapacity = value; return this; }
    public long getResponsiveTtlSeconds() { return responsiveTtlSeconds; }
    public DHTOptions setResponsiveTtlSeconds(long value) { this.responsiveTtlSeconds = value; return this; }
    public int getPoolLowWatermark() { return poolLowWatermark; }
    public DHTOptions setPoolLowWatermark(int value) { this.poolLowWatermark = value; return this; }
    public int getMaxInFlightPerSubnet() { return maxInFlightPerSubnet; }
    public DHTOptions setMaxInFlightPerSubnet(int value) { this.maxInFlightPerSubnet = value; return this; }

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
