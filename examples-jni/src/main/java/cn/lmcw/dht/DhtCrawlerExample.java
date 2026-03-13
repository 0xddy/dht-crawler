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
        DHTOptions options = new DHTOptions()
                .setPort(6881)
                .setNetMode(0)
                .setMetadataTimeout(5L)
                .setMaxMetadataQueueSize(50_000)
                .setMaxMetadataWorkerCount(500);

        AtomicLong torrentCount = new AtomicLong();
        CountDownLatch shutdown = new CountDownLatch(1);

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

            @Override
            public boolean onMetadataFetch(String infoHash) {
                return true;
            }
        };

        final DhtCrawler crawler = DhtCrawler.createServer(options, listener);
        crawler.start();
        System.out.println("DHT 已启动，端口 " + options.getPort());

        Runtime.getRuntime().addShutdownHook(new Thread(() -> {
            System.out.println("\n收到退出信号，正在停止...");
            crawler.stop();
            System.out.println("已关闭，共发现种子：" + torrentCount.get());
            shutdown.countDown();
        }));

        System.out.println("按 Ctrl+C 退出。每 10 秒打印一次节点池大小...");
        while (true) {
            Thread.sleep(10_000);
            System.out.println("节点池: " + crawler.getNodePoolSize()
                    + "  已发现种子: " + torrentCount.get());
        }
    }
}
