package cn.lmcw.dht;

import cn.lmcw.dht.model.DHTOptions;

/**
 * DHT 爬虫会话：封装 native 句柄与生命周期，推荐使用的面向对象入口。
 *
 * <pre>{@code
 * try (DhtCrawler crawler = DhtCrawler.createServer(options, listener)) {
 *     crawler.start();
 *     // ... 运行 ...
 * } // close → stop，释放 Rust 资源
 *
 * // 或手动：
 * DhtCrawler c = DhtCrawler.createServer(options, listener);
 * c.start();
 * // ...
 * c.stop();
 * }</pre>
 */
public final class DhtCrawler implements AutoCloseable {

    private long handle;
    private volatile boolean started;

    private DhtCrawler(long handle) {
        this.handle = handle;
    }

    /**
     * 创建 DHT 服务器会话（尚未 {@link #start()}，与 native createServer 对应）。
     * Java 中方法不能命名为 {@code new}，故用 {@code createServer}。
     *
     * @param options  配置，{@code null} 使用 Rust 默认
     * @param listener 回调，{@code null} 不注册回调
     * @return 已绑定 native 资源的会话
     * @throws IllegalStateException 创建失败（句柄为 0，通常伴随 JVM 异常）
     */
    public static DhtCrawler createServer(DHTOptions options, DhtListener listener) {
        long h = DhtCrawlerJni.createServer(options, listener);
        if (h == 0) {
            throw new IllegalStateException("DHT 服务器创建失败（createServer 返回 0）");
        }
        return new DhtCrawler(h);
    }

    /**
     * 在后台启动 DHT（非阻塞）。可多次调用，仅首次生效。
     *
     * @throws IllegalStateException 已 {@link #stop()} 关闭后不能再启动
     */
    public synchronized void start() {
        if (handle == 0) {
            throw new IllegalStateException("会话已关闭，无法 start");
        }
        if (started) {
            return;
        }
        DhtCrawlerJni.startServer(handle);
        started = true;
    }

    /**
     * 停止并释放全部 Rust 资源（tokio runtime 等）。幂等：重复调用安全。
     */
    public synchronized void stop() {
        if (handle == 0) {
            return;
        }
        DhtCrawlerJni.stopServer(handle);
        handle = 0;
        started = false;
    }

    /** 是否已调用过 {@link #start()} 且尚未 {@link #stop()}。 */
    public synchronized boolean isStarted() {
        return started && handle != 0;
    }

    /** 会话仍有效（未 stop）时为 true。 */
    public synchronized boolean isOpen() {
        return handle != 0;
    }

    /**
     * 当前 routing table 节点数；已关闭时返回 0。
     */
    public int getNodePoolSize() {
        long h = handle;
        if (h == 0) {
            return 0;
        }
        return DhtCrawlerJni.getNodePoolSize(h);
    }

    @Override
    public void close() {
        stop();
    }
}
