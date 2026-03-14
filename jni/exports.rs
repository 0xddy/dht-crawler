use crate::jni_bindings::callbacks::register_callbacks;
use crate::jni_bindings::env::JavaCallback;
use crate::jni_bindings::server::{handle_ref, into_handle_ptr, take_handle, ServerHandle};
use crate::jni_bindings::types::java_to_dht_options_or_default;
use jni::JNIEnv;
use jni::objects::{JClass, JObject};
use jni::sys::{jint, jlong};

// ──────────────────────────────────────────────────────────────────────────────
// cn.lmcw.dht.DhtCrawlerJni 的 JNI 导出
// ──────────────────────────────────────────────────────────────────────────────

/// 创建 DHTServer 并返回句柄（jlong）。
///
/// Java 签名：`native long createServer(DHTOptions options, DhtListener listener);`
///
/// - `options`：`cn.lmcw.dht.model.DHTOptions` 对象，或 null 则使用默认选项。
/// - `listener`：`cn.lmcw.dht.DhtListener` 实现，或 null 则不注册回调。
/// - 返回：服务器句柄（成功）或 0（失败）。
#[unsafe(no_mangle)]
pub extern "system" fn Java_cn_lmcw_dht_DhtCrawlerJni_createServer(
    mut env: JNIEnv,
    _class: JClass,
    options: JObject,
    listener: JObject,
) -> jlong {
    crate::jni_catch!(&mut env, 0, {
        // 解析选项
        let opts = match java_to_dht_options_or_default(&mut env, &options) {
            Ok(o) => o,
            Err(e) => {
                let _ = env.throw_new("java/lang/IllegalArgumentException", &e.to_string());
                return 0;
            }
        };

        // 创建 ServerHandle（初始化 runtime + DHTServer）
        let handle = match ServerHandle::new(opts) {
            Ok(h) => h,
            Err(e) => {
                let _ = env.throw_new("java/lang/RuntimeException", &e);
                return 0;
            }
        };

        // 若提供了 listener，注册回调
        if !listener.is_null() {
            match JavaCallback::new(&mut env, &listener) {
                Ok(cb) => register_callbacks(&handle.server, cb),
                Err(e) => {
                    let _ = env.throw_new("java/lang/RuntimeException", &e.to_string());
                    return 0;
                }
            }
        }

        into_handle_ptr(handle)
    })
}

/// 启动 DHTServer（在后台 tokio 任务中运行，不阻塞 JNI 线程）。
///
/// Java 签名：`native void startServer(long handle);`
#[unsafe(no_mangle)]
pub extern "system" fn Java_cn_lmcw_dht_DhtCrawlerJni_startServer(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    crate::jni_catch!(&mut env, (), {
        let h = unsafe {
            match handle_ref(handle) {
                Some(h) => h,
                None => {
                    let _ = env.throw_new("java/lang/IllegalArgumentException", "无效的服务器句柄");
                    return;
                }
            }
        };
        if let Err(e) = h.start() {
            let _ = env.throw_new("java/lang/RuntimeException", &e);
        }
    });
}

/// 停止并销毁 DHTServer：发关闭信号后释放 tokio runtime 等全部 Rust 资源。
/// 调用后句柄失效，勿再传入任何 native 方法。
///
/// Java 签名：`native void stopServer(long handle);`
#[unsafe(no_mangle)]
pub extern "system" fn Java_cn_lmcw_dht_DhtCrawlerJni_stopServer(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    jni_shutdown_and_release(&mut env, handle);
}

fn jni_shutdown_and_release(env: &mut JNIEnv, handle: jlong) {
    crate::jni_catch!(env, (), {
        if handle == 0 {
            return;
        }
        unsafe {
            match take_handle(handle) {
                Some(h) => h.shutdown_and_destroy_in_background(),
                None => {
                    let _ = env.throw_new("java/lang/IllegalArgumentException", "无效的服务器句柄");
                }
            }
        }
    });
}

/// 获取节点池当前大小。
///
/// Java 签名：`native int getNodePoolSize(long handle);`
#[unsafe(no_mangle)]
pub extern "system" fn Java_cn_lmcw_dht_DhtCrawlerJni_getNodePoolSize(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
) -> jint {
    crate::jni_catch!(&mut env, 0, {
        let h = unsafe {
            match handle_ref(handle) {
                Some(h) => h,
                None => return 0,
            }
        };
        h.node_pool_size() as jint
    })
}
