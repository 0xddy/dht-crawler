package cn.lmcw.dht;

import cn.lmcw.dht.model.DHTOptions;

/**
 * 包内 JNI 绑定（由 {@link DhtCrawler} 使用）。业务代码请用 {@link DhtCrawler}。
 */
final class DhtCrawlerJni {

    static {
        System.loadLibrary("dht_crawler");
    }

    private DhtCrawlerJni() {}

    static native long createServer(DHTOptions options, DhtListener listener);

    static native void startServer(long handle);

    static native void stopServer(long handle);

    static native int getNodePoolSize(long handle);
}
