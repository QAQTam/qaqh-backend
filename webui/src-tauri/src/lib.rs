//! QAQ-Harness 桌面壳(Tauri 2 宿主)入口。
//!
//! 架构:webview( Solid 渲染层)↔ 类型化 IPC ↔ Rust 宿主 ↔ `qaqh-client` ↔
//! daemon(sidecar 或共享在跑实例)。daemon token 只存在于宿主进程内存。

mod challenge;
mod commands;
mod daemon;
mod events;

use tauri::{Manager, RunEvent};

pub fn run() {
    tauri::Builder::default()
        // 单实例(D1):多开窗口聚焦到既有实例,宿主状态不复制。
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .manage(daemon::HostState::new())
        .invoke_handler(tauri::generate_handler![
            commands::session_list,
            commands::attach,
            commands::pending_approvals,
            commands::respond_approval,
            commands::send_message,
            commands::cancel_turn,
            commands::create_session,
            commands::timeline_page,
            commands::session_bootstrap,
            commands::service_rpc,
            commands::open_external,
            commands::streams_retry,
            commands::stop_stale_daemon,
        ])
        .setup(|app| {
            // 启动即校验/拉起 daemon(D1);不阻塞窗口显示,失败经事件上报。
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Err(error) = daemon::ensure_connected(&handle).await {
                    log::warn!("[qaqh-webui-app] initial daemon connect: {error}");
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("failed to build qaqh-webui-app")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                // 窗口关闭 ≠ 取消 Turn/审批(§5.3):detach(停止宿主后台任务),
                // daemon 与其会话继续存活。
                app.state::<daemon::HostState>().detach();
            }
        });
}
