package cn.lmcw.dht.model;

/**
 * 与 Rust {@code FileInfo} 一一对应的 POJO。
 * <p>表示一个种子内单个文件的路径与大小。</p>
 */
public final class FileInfo {

    private final String path;
    private final long size;

    /**
     * 供 JNI 层调用的全参构造器。
     *
     * @param path 文件相对路径
     * @param size 文件字节数
     */
    public FileInfo(String path, long size) {
        this.path = path;
        this.size = size;
    }

    public String getPath() { return path; }
    public long getSize() { return size; }

    @Override
    public String toString() {
        return "FileInfo{path='" + path + "', size=" + size + '}';
    }
}
