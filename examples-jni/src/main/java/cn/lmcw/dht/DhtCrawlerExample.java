package cn.lmcw.dht;

import cn.lmcw.dht.model.DHTOptions;
import cn.lmcw.dht.model.TorrentInfo;

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicLong;

/**
 * DHT-Crawler JNI 使用示例。
 *
 * <h3>运行前准备</h3>
 * <ol>
 *   <li>在仓库根目录编译 Rust JNI 产物：
 *       <pre>cargo build --release --features jni</pre>
 *   </li>
 *   <li>在 {@code examples-jni/} 目录下运行（{@code lib.path} 指向 so/dll/dylib 所在目录）：
 *       <pre>gradle run -Plib.path=../target/release</pre>
 *       或直接用默认路径（{@code ../target/release}）：
 *       <pre>gradle run</pre>
 *   </li>
 * </ol>
 */
public class DhtCrawlerExample {

    public static void main(String[] args) throws InterruptedException {
        // 1. 构造配置
        DHTOptions options = new DHTOptions()
                .setPort(6881)
                .setNetMode(0)              // 0 = IPv4 Only
                .setMetadataTimeout(5L)
                .setMaxMetadataQueueSize(50_000)
                .setMaxMetadataWorkerCount(500);

        AtomicLong torrentCount = new AtomicLong();
        CountDownLatch shutdown = new CountDownLatch(1);

        // 2. 实现回调
        DhtListener listener = new DhtListener() {
            @Override
            public void onTorrent(TorrentInfo info) {
                long n = torrentCount.incrementAndGet();
                System.out.printf("[#%d] %s  (%s)  files=%d%n",
                        n, info.getName(), info.getInfoHash(),
                        info.getFiles() != null ? info.getFiles().size() : 0);
            }

            @Override
            public void onError(String message) {
                System.err.println("[ERROR] " + message);
            }
        };

        // 3. 创建并启动服务器
        long handle = DhtCrawlerJni.createServer(options, listener);
        if (handle == 0) {
            System.err.println("创建服务器失败");
            return;
        }
        System.out.println("DHT 服务器已创建，正在启动...");
        DhtCrawlerJni.startServer(handle);
        System.out.println("DHT 服务器已启动，端口 " + options.getPort());

        // 4. 注册 JVM 关闭钩子，确保资源释放
        Runtime.getRuntime().addShutdownHook(new Thread(() -> {
            System.out.println("\n收到退出信号，正在停止...");
            DhtCrawlerJni.stopServer(handle);
            try { Thread.sleep(500); } catch (InterruptedException ignored) {}
            DhtCrawlerJni.destroyServer(handle);
            System.out.println("服务器已销毁，共发现种子：" + torrentCount.get());
            shutdown.countDown();
        }));

        // 5. 定期打印节点池大小
        System.out.println("按 Ctrl+C 退出。每 10 秒打印一次节点池大小...");
        while (true) {
            Thread.sleep(10_000);
            int poolSize = DhtCrawlerJni.getNodePoolSize(handle);
            System.out.println("节点池大小: " + poolSize + "  已发现种子: " + torrentCount.get());
        }
    }
}
