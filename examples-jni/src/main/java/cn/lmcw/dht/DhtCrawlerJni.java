package cn.lmcw.dht;

import cn.lmcw.dht.model.DHTOptions;

/**
 * Rust DHT-Crawler 库的 JNI 绑定入口。
 *
 * <p>所有方法均为 {@code static native}，通过 JNI 调用 Rust 实现。
 * 加载顺序由静态初始化块保证，使用前无需手动调用。</p>
 *
 * <h3>生命周期</h3>
 * <pre>{@code
 * long handle = DhtCrawlerJni.createServer(options, listener);
 * DhtCrawlerJni.startServer(handle);
 * // ... 运行中 ...
 * DhtCrawlerJni.stopServer(handle);
 * DhtCrawlerJni.destroyServer(handle); // 必须调用，否则 Rust 资源泄漏
 * }</pre>
 */
public final class DhtCrawlerJni {

    static {
        System.loadLibrary("dht_crawler");
    }

    private DhtCrawlerJni() {}

    /**
     * 创建并初始化 DHT 服务器，返回 Rust 侧句柄。
     *
     * @param options  服务器配置，传 {@code null} 则使用 Rust 侧默认值
     * @param listener 事件回调，传 {@code null} 则不注册任何回调
     * @return 服务器句柄（非 0 表示成功），或 0（失败，同时会向 JVM 抛出异常）
     */
    public static native long createServer(DHTOptions options, DhtListener listener);

    /**
     * 启动已创建的 DHT 服务器（非阻塞）。
     * 服务器在 Rust tokio runtime 后台运行，此方法立即返回。
     *
     * @param handle {@link #createServer} 返回的句柄
     */
    public static native void startServer(long handle);

    /**
     * 向服务器发送停止信号（非阻塞）。
     * 调用后服务器将优雅退出，但资源尚未释放，需继续调用 {@link #destroyServer}。
     *
     * @param handle {@link #createServer} 返回的句柄
     */
    public static native void stopServer(long handle);

    /**
     * 销毁服务器并释放所有 Rust 侧资源（tokio runtime、连接等）。
     * <strong>必须调用</strong>，否则发生内存泄漏。调用后不得再使用该句柄。
     *
     * @param handle {@link #createServer} 返回的句柄
     */
    public static native void destroyServer(long handle);

    /**
     * 获取当前节点池（routing table）中的节点数量。
     *
     * @param handle {@link #createServer} 返回的句柄
     * @return 节点数量，句柄无效时返回 0
     */
    public static native int getNodePoolSize(long handle);
}
