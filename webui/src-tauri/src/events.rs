//! `ClientHandlers` 回调 → Tauri 事件转发。
//!
//! payload 与现行 SSE 帧同形(plan §3 事件面):
//!  - `timeline://entry`    ← `on_timeline_entry`: `{ session_id, entry }`
//!  - `timeline://status`   ← `on_timeline_status`(serde tagged;前端 connection
//!    信号由此驱动,重连/退避/续传责任上移宿主)
//!  - `timeline://snapshot` ← `on_timeline_snapshot`(宿主侧缺口恢复的权威快照)
//!  - `projection://event`  ← `on_v2_event`(信封原样)
//!  - `projection://reset`  ← `on_v2_reset`(宿主 v2 流以 snapshot cursor 自动重订阅)
//!  - `conn://liveness`     ← `on_liveness`(节流,仅诊断)
//!
//! `on_v2_status` 不转发:V2Stream 的重连在宿主循环内自愈,connection 语义由
//! timeline 状态承担;转发它只会给 webview 增加无消费事件。
//!
//! 背压(plan B4):text_delta 帧未做宿主合并——Tauri 事件走本进程 webview
//! 通道,缺口→快照校正 + watermark 去重在前端兜底;`timeline_seq` 连续性由
//! 不动事件序列保证,任何合并优化都必须保持它。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use qaqh_client::ClientHandlers;
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter};

/// conn://liveness 的最小发射间隔:回调按 SSE 字节触发,webview 只需要
/// 低频「活着」信号。
const LIVENESS_MIN_INTERVAL_MS: u64 = 5_000;

static LAST_LIVENESS_MS: AtomicU64 = AtomicU64::new(0);

fn emit(app: &AppHandle, event: &str, payload: Value) {
    if let Err(error) = app.emit(event, payload) {
        log::warn!("[qaqh-webui-app] emit {event} failed: {error}");
    }
}

pub fn build_handlers(app: AppHandle) -> ClientHandlers {
    ClientHandlers {
        on_timeline_entry: {
            let app = app.clone();
            Arc::new(move |seed, entry| {
                if let Ok(entry) = serde_json::to_value(&entry) {
                    emit(
                        &app,
                        "timeline://entry",
                        json!({ "session_id": seed, "entry": entry }),
                    );
                }
            })
        },
        on_timeline_status: {
            let app = app.clone();
            Arc::new(move |status| {
                if let Ok(payload) = serde_json::to_value(&status) {
                    emit(&app, "timeline://status", payload);
                }
            })
        },
        on_timeline_snapshot: {
            let app = app.clone();
            Arc::new(move |page| {
                if let Ok(payload) = serde_json::to_value(&page) {
                    emit(&app, "timeline://snapshot", payload);
                }
            })
        },
        on_v2_event: {
            let app = app.clone();
            Arc::new(move |seed, event| {
                // 信封原样转发(cursor/session_id/stream_key/payload 均在信封内);
                // session_id 单独取一次供前端过滤。
                match serde_json::to_value(&event) {
                    Ok(mut envelope) => {
                        if envelope.get("session_id").is_none()
                            && let Some(object) = envelope.as_object_mut()
                        {
                            object.insert("session_id".into(), json!(seed));
                        }
                        emit(&app, "projection://event", envelope);
                    }
                    Err(error) => log::warn!("[qaqh-webui-app] serialize v2 event failed: {error}"),
                }
            })
        },
        on_v2_reset: {
            let app = app.clone();
            Arc::new(move |seed, reset| match serde_json::to_value(&reset) {
                Ok(mut payload) => {
                    if payload.get("session_id").is_none()
                        && let Some(object) = payload.as_object_mut()
                    {
                        object.insert("session_id".into(), json!(seed));
                    }
                    emit(&app, "projection://reset", payload);
                }
                Err(error) => log::warn!("[qaqh-webui-app] serialize v2 reset failed: {error}"),
            })
        },
        on_liveness: Arc::new(move || {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let last = LAST_LIVENESS_MS.load(Ordering::Relaxed);
            if now_ms.saturating_sub(last) < LIVENESS_MIN_INTERVAL_MS {
                return;
            }
            if LAST_LIVENESS_MS
                .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
            {
                return;
            }
            emit(&app, "conn://liveness", json!({}));
        }),
        // on_v2_status 不转发(见模块注释),保持 no-op 默认值。
        ..Default::default()
    }
}
