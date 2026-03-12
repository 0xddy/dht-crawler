use jni::{JavaVM, JNIEnv};
use jni::objects::{JObject, JValue};
use std::sync::{Arc, Mutex};

/// 跨线程安全的 Java 回调持有者。
/// 保存 JavaVM 指针和 Java 侧 listener 的全局引用，
/// 在任意 Rust 线程中都可 attach 并回调 Java 方法。
#[derive(Clone)]
pub struct JavaCallback {
    vm: Arc<JavaVM>,
    /// listener 对象的全局引用（不可跨线程直接用，需通过 vm 先 attach）
    listener: Arc<Mutex<jni::objects::GlobalRef>>,
}

impl JavaCallback {
    /// 从当前 JNI 调用线程创建，把 listener jobject 升级为全局引用。
    pub fn new(env: &mut JNIEnv, listener: &JObject) -> jni::errors::Result<Self> {
        let vm = env.get_java_vm()?;
        let global_ref = env.new_global_ref(listener)?;
        Ok(Self {
            vm: Arc::new(vm),
            listener: Arc::new(Mutex::new(global_ref)),
        })
    }

    /// 在任意线程中 attach JVM，执行 `f(env, listener)` 后自动 detach。
    /// `f` 内可调用 Java 方法；出错时 f 应检查 exception 并返回 Err。
    pub fn with_env<F, R>(&self, f: F) -> jni::errors::Result<R>
    where
        F: FnOnce(&mut JNIEnv, &JObject) -> jni::errors::Result<R>,
    {
        let mut guard = self.vm.attach_current_thread()?;
        let listener_guard = self.listener.lock().unwrap();
        let result = f(&mut guard, listener_guard.as_obj());
        // 检查并清除 Java 侧遗留的异常，避免后续 JNI 调用受污染
        if let Err(ref _e) = result {
            if guard.exception_check().unwrap_or(false) {
                let _ = guard.exception_clear();
            }
        }
        result
    }

    /// 调用 Java listener 的方法（用于简单通知）。
    pub fn call_void_method(
        &self,
        method_name: &str,
        sig: &str,
        args: &[JValue],
    ) -> jni::errors::Result<()> {
        self.with_env(|env, obj| {
            env.call_method(obj, method_name, sig, args)?;
            Ok(())
        })
    }
}

/// 安全地执行 JNI 代码块，捕获 Rust panic，避免 JVM 崩溃。
/// 若发生 panic，向 Java 抛出 RuntimeException 后返回 `default`。
#[macro_export]
macro_rules! jni_catch {
    ($env:expr, $default:expr, $body:expr) => {{
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| $body)) {
            Ok(v) => v,
            Err(_) => {
                let _ = $env.throw_new("java/lang/RuntimeException", "Rust internal panic");
                $default
            }
        }
    }};
}
