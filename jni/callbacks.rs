use crate::DHTServer;
use crate::jni_bindings::env::JavaCallback;
use crate::jni_bindings::types::torrent_info_to_java;
use jni::objects::JValue;
use std::sync::Arc;

/// 向 `DHTServer` 注册所有 Java 回调（on_torrent、on_error、on_metadata_fetch）。
///
/// `callback` 持有 Java listener 的全局引用，可跨线程安全使用。
pub fn register_callbacks(server: &Arc<DHTServer>, callback: JavaCallback) {
    register_on_torrent(server, callback.clone());
    register_on_error(server, callback.clone());
    register_on_metadata_fetch(server, callback);
}

/// 注册 on_torrent 回调：Rust TorrentInfo → Java listener.onTorrent(TorrentInfo)
fn register_on_torrent(server: &Arc<DHTServer>, callback: JavaCallback) {
    server.on_torrent(move |info| {
        let result = callback.with_env(|env, listener| {
            // 映射为 Java TorrentInfo 对象
            let j_info = torrent_info_to_java(env, &info).map_err(|e| {
                log::error!("TorrentInfo 转换 Java 对象失败: {e}");
                e
            })?;
            env.call_method(
                listener,
                "onTorrent",
                "(Lcn/lmcw/dht/model/TorrentInfo;)V",
                &[JValue::Object(&j_info)],
            )?;
            Ok(())
        });
        if let Err(e) = result {
            log::error!("回调 onTorrent 失败: {e}");
        }
    });
}

/// 注册 on_error 回调：Rust DHTError → Java listener.onError(String)
fn register_on_error(server: &Arc<DHTServer>, callback: JavaCallback) {
    server.on_error(move |err| {
        let msg = err.to_string();
        let result = callback.with_env(|env, listener| {
            let j_msg = env.new_string(&msg)?;
            env.call_method(
                listener,
                "onError",
                "(Ljava/lang/String;)V",
                &[JValue::Object(&j_msg.into())],
            )?;
            Ok(())
        });
        if let Err(e) = result {
            log::error!("回调 onError 失败: {e}");
        }
    });
}

/// 注册 on_metadata_fetch：在拉取 metadata 前询问 Java `onMetadataFetch(infoHash)`。
/// 返回 `true` 才继续拉取；在阻塞线程池中执行 JNI，避免阻塞 tokio worker。
fn register_on_metadata_fetch(server: &Arc<DHTServer>, callback: JavaCallback) {
    server.on_metadata_fetch(move |info_hash| {
        let cb = callback.clone();
        async move {
            let r = tokio::task::spawn_blocking(move || {
                cb.with_env(|env, listener| {
                    let j_s = env.new_string(&info_hash)?;
                    let out = env.call_method(
                        listener,
                        "onMetadataFetch",
                        "(Ljava/lang/String;)Z",
                        &[JValue::Object(&j_s.into())],
                    )?;
                    Ok(out.z()?)
                })
            })
            .await;
            match r {
                Ok(Ok(allow)) => allow,
                Ok(Err(e)) => {
                    log::error!("回调 onMetadataFetch 失败: {e}，默认允许拉取");
                    true
                }
                Err(e) => {
                    log::error!("onMetadataFetch spawn_blocking 失败: {e}，默认允许拉取");
                    true
                }
            }
        }
    });
}
