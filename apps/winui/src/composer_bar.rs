//! XAML 原生 Composer 底部栏（P6 输入框迁移块）— Web `ComposerDock` 的壳侧承载。
//!
//! 布局（挂载于 main.rs right_content row0 内层 grid row1，chat 视图可见）：
//! ```text
//! ┌ goalBar（dashboard 投影：当前任务 + 状态计数）───────────────┐  ← 可选
//! ├ queue 行（"n 条后续任务已排队" + 列表 + 删除）              │  ← 可选
//! ├ slash 菜单（composer 上方覆盖层 cell）                      │  ← 可选
//! └ 卡片（LayerFill + 圆角 8px，对齐 .composer-dock）           │
//!   ├ TextBox（多行 + Enter accelerator 发送 / Shift+Enter 换行）│
//!   ├ submitError 行 / 附件预览行 / 附件菜单（图片|文本）        │
//!   └ footer：mode 切换 | 权限 pill | token/model | 发送↑/停止■  │
//! ```
//!
//! 数据源：`bridge.core().composer_snapshot()`（Web `shell.setComposer`
//! 投影，250ms rev 轮询）+ `dashboard_snapshot()`（control 事件投影，
//! goalBar）。`sendAck` 变化 → 清空草稿（悲观清空）；`seed` 变化 → 重置
//! 草稿（会话切换）。
//!
//! 草稿态（text/附件/slash 状态）为纯 UI 态：`use_ref` 真实存储 +
//! `use_state<u64>` 版本号触发重渲染（SetState 无 get，回调从 ref 读写，
//! UI 线程单线程安全）。提交时才经 `shell.composerAction` 进协议——
//! 每字符零同步（IME 原生），状态单源仍为 Web。
//!
//! 复刻偏差（对齐项目既有偏差记录风格）：
//! - textarea 62→180px 自动高度 → TextBox 固定高（72px + 滚动）
//! - 毛玻璃 backdrop-filter → LayerFill + 圆角（壳内统一，同 info_panel）
//! - 附件图片预览（object URL）→ 一期仅文件名 + 大小（阶段 B 临时文件）
//! - 附件/slash 菜单 absolute 定位 → 卡片内覆盖层 cell（语义等价）
//! - TodoStatusStrip 展开列表 → 计数徽标 + 当前任务行（二期展开）
//! - Enter 发送走 KeyboardAccelerator（reactor 无 KeyDown；Shift+Enter
//!   因带修饰键不匹配 accelerator → TextBox 默认换行保留）

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use windows_reactor::*;

use crate::bridge::{Bridge, ComposerAttachment, ComposerState, ComposerTextFile};
use crate::shell_store::DashboardSnapshot;

/// 快照轮询间隔（同 interaction_overlay：交互响应优先）。
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// 输入框高度（对齐 Web textarea 62px 起步视觉）。
const INPUT_HEIGHT: f64 = 72.0;
/// 等宽字体（附件路径/元信息；同 info_panel `MONO_FONT`）。
const MONO_FONT: &str = "Consolas";

/// 诊断日志（同 main.rs log_diag 约定：GUI 子系统无控制台，写文件）。
fn log_diag(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::var("DEEPX_WINUI_LOG").unwrap_or_else(|_| ".deepx-winui.log".into()))
    {
        let _ = writeln!(f, "[composer_bar] {msg}");
    }
}

/// 附件 id 计数器（同 Web makeImageId/makeTextId 的进程内唯一语义）。
static ATT_ID: AtomicU64 = AtomicU64::new(0);

// ── slash 命令（对齐 Web `slashCommands.ts` 常量表，纯展示）────────

const SLASH_COMMANDS: &[(&str, &str, &str)] = &[
    ("/settings", "设置", "打开应用设置"),
    ("/model", "模型", "切换对话模型"),
    ("/effort", "强度", "调整推理强度"),
    ("/usage", "用量", "查看用量图表"),
];

/// 匹配候选（对齐 Web `matchSlashCommands`：仅 "/" 开头时返回）。
fn match_slash_commands(value: &str) -> Vec<(String, String, String)> {
    if !value.starts_with('/') {
        return Vec::new();
    }
    let query = value[1..].trim().to_lowercase();
    SLASH_COMMANDS
        .iter()
        .filter(|(cmd, label, _)| {
            query.is_empty()
                || cmd[1..].to_lowercase().contains(&query)
                || label.to_lowercase().contains(&query)
        })
        .map(|(c, l, d)| (c.to_string(), l.to_string(), d.to_string()))
        .collect()
}

// ── 附件（本地草稿态）───────────────────────────────────────────

#[derive(Clone)]
enum AttachmentKind {
    Image { mime_type: String },
    Text,
}

#[derive(Clone)]
struct AttachmentItem {
    id: String,
    kind: AttachmentKind,
    file_name: String,
    size: u64,
    path: String,
    /// 图片缩略图临时文件路径（%TEMP%/deepx-preview-*；渲染转 file:// URI，
    /// 移除/清空时删除；仅 Image 附件有值）。
    preview_path: Option<String>,
}

impl AttachmentItem {
    fn size_label(&self) -> String {
        format_size(self.size)
    }
}

/// 字节大小格式化（对齐 Web `formatSize`：B/KB/MB 一位小数）。
fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// 复制图片到 %TEMP% 做预览源（WinUI Image 不支持 base64，用 file:// 加载）。
/// 返回临时文件路径；失败返回 None（预览降级为仅文件名，不影响发送）。
fn write_preview_copy(src: &str, id: &str) -> Option<String> {
    let ext = std::path::Path::new(src)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("png");
    let tmp = std::env::temp_dir().join(format!("deepx-preview-{id}.{ext}"));
    let tmp_str = tmp.to_string_lossy().to_string();
    std::fs::copy(src, &tmp).ok()?;
    Some(tmp_str)
}

/// 删除预览临时文件（移除附件 / 清空草稿时调用；失败静默，%TEMP% 系统可清）。
fn remove_preview(preview_path: Option<&str>) {
    if let Some(p) = preview_path {
        let _ = std::fs::remove_file(p);
    }
}

/// 草稿态（纯 UI，不进协议；提交时组装载荷）。
#[derive(Clone, Default)]
struct Draft {
    text: String,
    attachments: Vec<AttachmentItem>,
    attach_open: bool,
    selected_slash: usize,
    dismissed_slash: Option<String>,
}

/// 小标题（eyebrow：11px 600 muted，同 info_panel `section_heading`）。
fn eyebrow(text: &str) -> Element {
    text_block(text)
        .font_size(11.0)
        .semibold()
        .foreground(ThemeRef::SecondaryText)
        .into()
}

/// XAML Composer 底部栏（chat 视图；main.rs 内层 grid row1 挂载）。
pub fn composer_bar(cx: &mut RenderCx, bridge: Arc<Bridge>) -> Element {
    let (state, set_state) = cx.use_state::<ComposerState>(ComposerState::default());
    let timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    let last_rev = cx.use_ref::<u64>(0);
    // dashboard（goalBar）投影。
    let (dashboard, set_dashboard) = cx.use_state::<Option<DashboardSnapshot>>(None);
    let dash_timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    let last_dash_rev = cx.use_ref::<u64>(0);
    // 草稿：ref 真实存储 + 版本号驱动渲染（SetState 无 get，回调从 ref 读写）。
    let draft = cx.use_ref::<Draft>(Draft::default());
    let (_draft_version, set_draft_version) = cx.use_state::<u64>(0);
    // 版本号真实存储（回调读 ref 递增，避免批处理下基于渲染值 +1 丢帧）。
    let draft_ver = cx.use_ref::<u64>(0);
    // sendAck/seed 基线（effect 比对用）。
    let last_ack = cx.use_ref::<u64>(0);
    let last_seed = cx.use_ref::<String>(String::new());

    // composer 投影轮询（250ms rev 比对）。
    cx.use_effect((), {
        let bridge = bridge.clone();
        let set_state = set_state.clone();
        let timer = timer.clone();
        let last_rev = last_rev.clone();
        move || {
            crate::shell::poll_rev(
                timer,
                last_rev,
                POLL_INTERVAL,
                move || bridge.core().composer_snapshot(),
                move |s| set_state.call(s),
            );
        }
    });

    // goalBar dashboard 轮询（同模式；快照为空时 UI 不渲染）。
    cx.use_effect((), {
        let bridge = bridge.clone();
        let set_dashboard = set_dashboard.clone();
        let dash_timer = dash_timer.clone();
        let last_dash_rev = last_dash_rev.clone();
        move || {
            crate::shell::poll_rev(
                dash_timer,
                last_dash_rev,
                POLL_INTERVAL,
                move || bridge.core().dashboard_snapshot(),
                move |s| set_dashboard.call(s),
            );
        }
    });

    // sendAck 变化 → 发送已确认 → 清空草稿（悲观清空，对齐 Web 成功路径）。
    // seed 变化 → 会话切换 → 重置草稿（对齐 Web 新会话空输入）。
    // deps 与闭包都捕获 clone 值，避免 move 闭包拿走 state 所有权。
    let ack0 = state.send_ack;
    let seed0 = state.seed.clone();
    cx.use_effect((ack0, seed0.clone()), {
        let draft = draft.clone();
        let draft_ver = draft_ver.clone();
        let last_ack = last_ack.clone();
        let last_seed = last_seed.clone();
        let set_draft_version = set_draft_version.clone();
        move || {
            let ack = ack0;
            let seed = seed0.clone();
            if ack != *last_ack.borrow() {
                *last_ack.borrow_mut() = ack;
                if ack > 0 {
                    let mut d = draft.borrow_mut();
                    for att in &d.attachments {
                        remove_preview(att.preview_path.as_deref());
                    }
                    d.text.clear();
                    d.attachments.clear();
                    d.attach_open = false;
                    log_diag("sendAck: draft cleared");
                }
            }
            if seed != *last_seed.borrow() {
                *last_seed.borrow_mut() = seed;
                let mut d = draft.borrow_mut();
                for att in &d.attachments {
                    remove_preview(att.preview_path.as_deref());
                }
                d.text.clear();
                d.attachments.clear();
                d.attach_open = false;
                d.selected_slash = 0;
                d.dismissed_slash = None;
                log_diag("seed changed: draft reset");
            }
            let v = *draft_ver.borrow() + 1;
            *draft_ver.borrow_mut() = v;
            set_draft_version.call(v);
        }
    });

    // ── 回调（Arc 共享 + ref 读写；渲染时捕获 state 快照）───────────
    let has_pending_gate = state.has_pending_gate;
    let is_streaming = state.is_streaming;

    // 文本输入：更新草稿 + 重置 slash 导航（对齐 Web updateText）。
    let on_text_changed = {
        let draft = draft.clone();
        let draft_ver = draft_ver.clone();
        let set_draft_version = set_draft_version.clone();
        move |value: String| {
            let mut d = draft.borrow_mut();
            d.text = value;
            d.selected_slash = 0;
            d.dismissed_slash = None;
            drop(d);
            let v = *draft_ver.borrow() + 1;
            *draft_ver.borrow_mut() = v;
            set_draft_version.call(v);
        }
    };

    // 提交：校验 → emit Send（附件传路径，base64 由 Web 侧读）。
    let on_submit: Arc<dyn Fn() + 'static> = Arc::new({
        let bridge = bridge.clone();
        let draft = draft.clone();
        let draft_ver = draft_ver.clone();
        let set_draft_version = set_draft_version.clone();
        let has_pending_gate = has_pending_gate;
        move || {
            let d = draft.borrow();
            let text = d.text.trim().to_string();
            if (text.is_empty() && d.attachments.is_empty()) || has_pending_gate {
                return;
            }
            let mut image_paths: Vec<ComposerAttachment> = Vec::new();
            let mut text_files: Vec<ComposerTextFile> = Vec::new();
            for att in &d.attachments {
                match &att.kind {
                    AttachmentKind::Image { mime_type } => image_paths.push(ComposerAttachment {
                        file_name: att.file_name.clone(),
                        mime_type: mime_type.clone(),
                        path: att.path.clone(),
                    }),
                    AttachmentKind::Text => text_files.push(ComposerTextFile {
                        file_name: att.file_name.clone(),
                        path: att.path.clone(),
                    }),
                }
            }
            drop(d);
            log_diag("composer send emitted");
            // 直连动作：协议请求 Rust 直发（附件上传 ContentRef 后发命令）。
            bridge.spawn_send_message(text, image_paths, text_files);
            // 悲观清空（对齐 Web sendAck 语义）：提交即清空草稿/附件。
            {
                let mut d = draft.borrow_mut();
                d.text = String::new();
                d.attachments.clear();
                d.selected_slash = 0;
                d.dismissed_slash = None;
                drop(d);
                let v = *draft_ver.borrow() + 1;
                *draft_ver.borrow_mut() = v;
                set_draft_version.call(v);
            }
        }
    });

    // 附件选择（STA 直调对话框 + 读元数据；用户取消返回 null 不动作）。
    let on_pick_image = {
        let bridge = bridge.clone();
        let draft = draft.clone();
        let draft_ver = draft_ver.clone();
        let set_draft_version = set_draft_version.clone();
        move || match bridge.pick_image_file() {
            Ok(serde_json::Value::String(path)) => {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                let file_name = path
                    .split(['/', '\\'])
                    .last()
                    .unwrap_or("image")
                    .to_string();
                let mime_type = guess_image_mime(&file_name);
                let id = format!("att-{}-{}", ATT_ID.fetch_add(1, Ordering::Relaxed), size);
                // 缩略图预览：复制到 %TEMP%（WinUI Image 不支持 base64）。
                let preview_path = write_preview_copy(&path, &id);
                let mut d = draft.borrow_mut();
                d.attachments.push(AttachmentItem {
                    id,
                    kind: AttachmentKind::Image { mime_type },
                    file_name,
                    size,
                    path,
                    preview_path,
                });
                d.attach_open = false;
                drop(d);
                let v = *draft_ver.borrow() + 1;
                *draft_ver.borrow_mut() = v;
                set_draft_version.call(v);
            }
            _ => {}
        }
    };
    let on_pick_text = {
        let bridge = bridge.clone();
        let draft = draft.clone();
        let draft_ver = draft_ver.clone();
        let set_draft_version = set_draft_version.clone();
        move || match bridge.pick_text_file() {
            Ok(serde_json::Value::String(path)) => {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                let file_name = path.split(['/', '\\']).last().unwrap_or("file").to_string();
                let mut d = draft.borrow_mut();
                d.attachments.push(AttachmentItem {
                    id: format!("att-{}-{}", ATT_ID.fetch_add(1, Ordering::Relaxed), size),
                    kind: AttachmentKind::Text,
                    file_name,
                    size,
                    path,
                    preview_path: None,
                });
                d.attach_open = false;
                drop(d);
                let v = *draft_ver.borrow() + 1;
                *draft_ver.borrow_mut() = v;
                set_draft_version.call(v);
            }
            _ => {}
        }
    };
    // 移除附件（按 id；顺带删预览临时文件）。
    let on_remove_attach: Arc<dyn Fn(String) + 'static> = Arc::new({
        let draft = draft.clone();
        let draft_ver = draft_ver.clone();
        let set_draft_version = set_draft_version.clone();
        move |id: String| {
            let mut d = draft.borrow_mut();
            if let Some(att) = d.attachments.iter().find(|a| a.id == id) {
                remove_preview(att.preview_path.as_deref());
            }
            d.attachments.retain(|a| a.id != id);
            drop(d);
            let v = *draft_ver.borrow() + 1;
            *draft_ver.borrow_mut() = v;
            set_draft_version.call(v);
        }
    });
    // 附件菜单开合。
    let on_toggle_attach = {
        let draft = draft.clone();
        let draft_ver = draft_ver.clone();
        let set_draft_version = set_draft_version.clone();
        move || {
            let mut d = draft.borrow_mut();
            d.attach_open = !d.attach_open;
            drop(d);
            let v = *draft_ver.borrow() + 1;
            *draft_ver.borrow_mut() = v;
            set_draft_version.call(v);
        }
    };

    // footer 动作（直连动作：协议请求 Rust 直发，不再回传 Web）。
    let on_mode_toggle = {
        let bridge = bridge.clone();
        let mode = state.mode.clone();
        move || {
            let next = if mode == "plan" { "code" } else { "plan" };
            bridge.spawn_set_mode(next);
        }
    };
    let on_permission: Arc<dyn Fn(u64) + 'static> = Arc::new({
        let bridge = bridge.clone();
        move |level: u64| {
            bridge.spawn_set_permission(level);
        }
    });
    let on_stop = {
        let bridge = bridge.clone();
        move || {
            bridge.spawn_conversation_command(
                deepx_client::ConversationCommand::ConversationCancel { turn_id: None },
            )
        }
    };
    // 队列移除：queue 已随 B 组本地化恒空（本地无排队概念，WebView 移除），
    // 保留绑定签名兼容 queue_row，但无实际动作。
    let on_queue_remove: Arc<dyn Fn(String) + 'static> = Arc::new({
        let _bridge = bridge.clone();
        move |_id: String| {}
    });

    // ── 渲染时读取草稿（版本号变化触发本函数重跑）────────────────
    let d = draft.borrow();
    let text = d.text.clone();
    let attachments = d.attachments.clone();
    let attach_open = d.attach_open;
    let selected_slash = d.selected_slash;
    let dismissed = d.dismissed_slash.clone();
    // slash 菜单可见性（对齐 Web visibleSlashCommands）。
    let slash_cmds = if dismissed.as_deref() == Some(text.as_str()) {
        Vec::new()
    } else {
        match_slash_commands(&text)
    };
    let slash_visible = !slash_cmds.is_empty();
    drop(d);

    // Enter 发送 accelerator（菜单可见时 Enter = 选中命令，否则发送）。
    let on_enter = {
        let draft = draft.clone();
        let draft_ver = draft_ver.clone();
        let set_draft_version = set_draft_version.clone();
        let slash_cmds = slash_cmds.clone();
        let selected_slash = selected_slash;
        let on_submit = on_submit.clone();
        move || {
            if !slash_cmds.is_empty() {
                let idx = selected_slash % slash_cmds.len();
                let (cmd, _, _) = &slash_cmds[idx];
                let mut d = draft.borrow_mut();
                d.text = cmd.clone();
                d.selected_slash = 0;
                d.dismissed_slash = Some(cmd.clone());
                drop(d);
                let v = *draft_ver.borrow() + 1;
                *draft_ver.borrow_mut() = v;
                set_draft_version.call(v);
            } else {
                on_submit();
            }
        }
    };
    // 菜单导航（仅菜单可见时绑定 ↓↑ Esc）。
    let on_slash_next = {
        let draft = draft.clone();
        let draft_ver = draft_ver.clone();
        let set_draft_version = set_draft_version.clone();
        let len = slash_cmds.len();
        move || {
            let mut d = draft.borrow_mut();
            d.selected_slash = (d.selected_slash + 1) % len.max(1);
            drop(d);
            let v = *draft_ver.borrow() + 1;
            *draft_ver.borrow_mut() = v;
            set_draft_version.call(v);
        }
    };
    let on_slash_prev = {
        let draft = draft.clone();
        let draft_ver = draft_ver.clone();
        let set_draft_version = set_draft_version.clone();
        let len = slash_cmds.len();
        move || {
            let mut d = draft.borrow_mut();
            d.selected_slash = (d.selected_slash + len - 1) % len.max(1);
            drop(d);
            let v = *draft_ver.borrow() + 1;
            *draft_ver.borrow_mut() = v;
            set_draft_version.call(v);
        }
    };
    let on_slash_dismiss = {
        let draft = draft.clone();
        let draft_ver = draft_ver.clone();
        let set_draft_version = set_draft_version.clone();
        let text = text.clone();
        move || {
            let mut d = draft.borrow_mut();
            d.dismissed_slash = Some(text.clone());
            drop(d);
            let v = *draft_ver.borrow() + 1;
            *draft_ver.borrow_mut() = v;
            set_draft_version.call(v);
        }
    };
    // 菜单项点击选中。
    let on_slash_pick: Arc<dyn Fn(String) + 'static> = Arc::new({
        let draft = draft.clone();
        let draft_ver = draft_ver.clone();
        let set_draft_version = set_draft_version.clone();
        move |cmd: String| {
            let mut d = draft.borrow_mut();
            d.text = cmd.clone();
            d.selected_slash = 0;
            d.dismissed_slash = Some(cmd);
            drop(d);
            let v = *draft_ver.borrow() + 1;
            *draft_ver.borrow_mut() = v;
            set_draft_version.call(v);
        }
    });

    // ── 渲染树 ───────────────────────────────────────────────────
    let placeholder = if has_pending_gate {
        "请先处理当前授权请求"
    } else {
        "向 DeepX 提问…"
    };

    // TextBox + Enter accelerator（菜单可见时附加 ↓↑ Esc）。
    let mut input: Element = text_box(text.clone())
        .accepts_return(true)
        .placeholder_text(placeholder)
        .height(INPUT_HEIGHT)
        .on_text_changed(on_text_changed)
        .keyboard_accelerator(KeyboardAccelerator::new(
            VirtualKey::Enter,
            VirtualKeyModifiers::None,
            on_enter,
        ))
        .into();
    if slash_visible {
        input = input
            .keyboard_accelerator(KeyboardAccelerator::new(
                VirtualKey::Down,
                VirtualKeyModifiers::None,
                on_slash_next,
            ))
            .keyboard_accelerator(KeyboardAccelerator::new(
                VirtualKey::Up,
                VirtualKeyModifiers::None,
                on_slash_prev,
            ))
            .keyboard_accelerator(KeyboardAccelerator::new(
                VirtualKey::Escape,
                VirtualKeyModifiers::None,
                on_slash_dismiss,
            ));
    }

    // 附件预览行（图片：缩略图 + 文件名；文本：图标 + 文件名；均含大小与移除）。
    let mut attach_rows: Vec<Element> = Vec::new();
    for (i, att) in attachments.iter().enumerate() {
        let icon = match att.kind {
            AttachmentKind::Image { .. } => "🖼️",
            AttachmentKind::Text => "📄",
        };
        // 图片缩略图：file:// URI 加载 %TEMP% 预览副本（48x48，等比裁切）。
        let thumb: Element = match (&att.kind, &att.preview_path) {
            (AttachmentKind::Image { .. }, Some(p)) => {
                let uri = format!("file:///{}", p.replace('\\', "/"));
                border(
                    Image::new_with_uri(uri)
                        .width(48.0)
                        .height(48.0)
                        .stretch(Stretch::UniformToFill),
                )
                .corner_radius(4.0)
                .into()
            }
            _ => text_block(icon).font_size(16.0).into(),
        };
        let row: Element = border(
            hstack((
                thumb,
                text_block(format!("{} ({})", att.file_name, att.size_label()))
                    .font_size(12.0)
                    .font_family(MONO_FONT)
                    .foreground(ThemeRef::SecondaryText),
                button("×").subtle().on_click({
                    let on_remove_attach = on_remove_attach.clone();
                    let id = att.id.clone();
                    move || on_remove_attach(id.clone())
                }),
            ))
            .spacing(8.0),
        )
        .background(ThemeRef::ControlFillSecondary)
        .corner_radius(4.0)
        .padding(4.0)
        .into();
        attach_rows.push(row.with_key(format!("att-{i}-{}", att.id)));
    }
    let attach_preview: Element = if attach_rows.is_empty() {
        grid(()).into()
    } else {
        vstack(attach_rows).spacing(4.0).into()
    };

    // 附件菜单（attach_open 时在 footer 上方）。
    let attach_menu: Element = if attach_open {
        hstack((
            button("🖼️ 上传图片").subtle().on_click(on_pick_image),
            button("📄 上传文本").subtle().on_click(on_pick_text),
        ))
        .spacing(8.0)
        .into()
    } else {
        grid(()).into()
    };

    // submitError 行（Web 失败回填；壳保留草稿不清空）。
    let error_row: Element = if state.submit_error.is_empty() {
        grid(()).into()
    } else {
        text_block(&state.submit_error)
            .font_size(12.0)
            .foreground(ThemeRef::SystemCritical)
            .wrap()
            .into()
    };

    // 发送/停止 + 元信息。
    let can_send = (!text.trim().is_empty() || !attachments.is_empty()) && !has_pending_gate;
    let meta: Element = hstack((
        text_block(format!("{:.1}K", state.context_tokens as f64 / 1000.0))
            .font_size(12.0)
            .foreground(ThemeRef::SecondaryText),
        text_block(&state.model)
            .font_size(12.0)
            .foreground(ThemeRef::SecondaryText),
    ))
    .spacing(8.0)
    .into();
    let send_stop: Element = if is_streaming {
        button("■").subtle().on_click(on_stop).into()
    } else {
        button("↑")
            .accent()
            .enabled(can_send)
            .on_click({
                let cb = on_submit.clone();
                move || cb()
            })
            .into()
    };

    // 权限 pill（对齐 Web PermissionLevelSelect 4 档；active 用 accent）。
    let mut pills: Vec<Element> = Vec::new();
    for (value, label) in [(1u64, "L1"), (2u64, "L2"), (3u64, "L3"), (4u64, "L4")] {
        let active = state.permission_level == value;
        let mut pill = button(label).subtle().on_click({
            let on_permission = on_permission.clone();
            move || on_permission(value)
        });
        if active {
            pill = pill.accent();
        }
        pills.push(pill.into());
    }

    // footer 行：左（附件 + mode）| 右（元信息 + 发送/停止）。
    let footer: Element = hstack((
        button("＋").subtle().on_click(on_toggle_attach),
        button(if state.mode == "plan" {
            "规划"
        } else {
            "执行"
        })
        .subtle()
        .on_click(on_mode_toggle),
        grid(()).horizontal_alignment(HorizontalAlignment::Stretch),
        vstack((meta, hstack(pills).spacing(4.0))).spacing(4.0),
        send_stop,
    ))
    .spacing(8.0)
    .into();

    // 卡片。
    let card: Element = border(
        vstack((input, error_row, attach_preview, attach_menu, footer))
            .spacing(8.0)
            .padding(12.0),
    )
    .corner_radius(8.0)
    .background(ThemeRef::LayerFill)
    .into();

    // goalBar（dashboard 非空时）。
    let goal_bar: Element = match dashboard.as_ref() {
        Some(snap) if !snap.tasks.is_empty() => goal_bar_row(snap),
        _ => grid(()).into(),
    };

    // 队列行（queue_count > 0 时）。
    let queue_bar: Element = if state.queue_count > 0 {
        queue_row(&state, on_queue_remove)
    } else {
        grid(()).into()
    };

    // slash 菜单（可见时；composer 卡片上方 cell）。
    let slash_menu: Element = if slash_visible {
        let mut items: Vec<Element> = Vec::new();
        for (i, (cmd, label, desc)) in slash_cmds.iter().enumerate() {
            let selected = i == selected_slash;
            let mut opt = button(format!("{cmd}  {label}  {desc}"))
                .subtle()
                .on_click({
                    let on_slash_pick = on_slash_pick.clone();
                    let cmd = cmd.clone();
                    move || on_slash_pick(cmd.clone())
                });
            if selected {
                opt = opt.accent();
            }
            items.push(opt.into());
        }
        border(vstack(items).spacing(2.0).padding(6.0))
            .corner_radius(6.0)
            .background(ThemeRef::CardBackground)
            .into()
    } else {
        grid(()).into()
    };

    vstack((goal_bar, queue_bar, slash_menu, card))
        .spacing(8.0)
        .padding(16.0)
        .into()
}

/// goalBar 简化行：当前任务（subject + status）+ 状态计数。
fn goal_bar_row(snap: &DashboardSnapshot) -> Element {
    let current = snap
        .current_todo_id
        .as_deref()
        .and_then(|id| snap.tasks.iter().find(|t| t.id == id));
    let pending = snap.tasks.iter().filter(|t| t.status == "pending").count();
    let in_progress = snap
        .tasks
        .iter()
        .filter(|t| t.status == "in_progress")
        .count();
    let done = snap
        .tasks
        .iter()
        .filter(|t| t.status == "completed")
        .count();
    let mut parts: Vec<Element> = Vec::new();
    if let Some(task) = current {
        parts.push(
            hstack((
                eyebrow("当前任务"),
                text_block(&task.subject).font_size(12.0).semibold().wrap(),
                text_block(&task.status)
                    .font_size(11.0)
                    .foreground(ThemeRef::SystemCaution),
            ))
            .spacing(8.0)
            .into(),
        );
    }
    parts.push(
        text_block(format!(
            "待处理 {pending} · 进行中 {in_progress} · 已完成 {done}"
        ))
        .font_size(11.0)
        .foreground(ThemeRef::SecondaryText)
        .into(),
    );
    border(hstack(parts).spacing(16.0).padding(8.0))
        .corner_radius(6.0)
        .background(ThemeRef::CardBackground)
        .into()
}

/// 队列行："n 条后续任务已排队" + items + 删除。
fn queue_row(state: &ComposerState, on_queue_remove: Arc<dyn Fn(String) + 'static>) -> Element {
    let mut items: Vec<Element> = Vec::new();
    for (i, item) in state.queue_items.iter().enumerate() {
        let row: Element = hstack((
            text_block(&item.text)
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText)
                .wrap(),
            button("×").subtle().on_click({
                let on_queue_remove = on_queue_remove.clone();
                let id = item.id.clone();
                move || on_queue_remove(id.clone())
            }),
        ))
        .spacing(8.0)
        .into();
        items.push(row.with_key(format!("q-{i}-{}", item.id)));
    }
    border(
        vstack((
            text_block(format!("{} 条后续任务已排队", state.queue_count))
                .font_size(12.0)
                .semibold(),
            vstack(items).spacing(4.0),
        ))
        .spacing(6.0)
        .padding(10.0),
    )
    .corner_radius(6.0)
    .background(ThemeRef::CardBackground)
    .into()
}

/// 图片 MIME 猜测（对齐 Web 端 Blob type 语义；对话框无 mime 信息）。
fn guess_image_mime(file_name: &str) -> String {
    let lower = file_name.to_lowercase();
    if lower.ends_with(".png") {
        "image/png".to_string()
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg".to_string()
    } else if lower.ends_with(".gif") {
        "image/gif".to_string()
    } else if lower.ends_with(".webp") {
        "image/webp".to_string()
    } else if lower.ends_with(".bmp") {
        "image/bmp".to_string()
    } else {
        "image/*".to_string()
    }
}
