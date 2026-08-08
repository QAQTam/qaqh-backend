//! XAML 原生标题栏（P0）— ThreadHeader 的壳侧承载（PLAN-NATIVE-UI.md）。
//!
//! 布局：
//!   TitleBar（SetTitleBar 拖拽区，host 自动接线 host.rs:277-288）
//!   ├── title 槽：TextBlock（会话标题 / 视图名，shell.header 推送）
//!   └── footer 槽：hstack( ①workspace ②location ③console ┃ ④info ⑤stats ⑥undo ⑦compact )
//!        —— ⑧pet 不渲染（壳 stub 恒 false，规划决策）
//!
//! 状态：timer 轮询 `core.header_snapshot()` rev（同 sidebar 500ms 模式，
//! 经 `shell::poll_rev` helper，P-4）。
//! 动作：①②③ 壳直接处理（目录对话框 / 系统 shell / DevTools）；
//!       ④-⑦ emit `shell.headerAction` 回传 Web 执行（状态单一数据源在 Web）。

use std::sync::Arc;
use std::time::Duration;

use windows_reactor::*;

use crate::bridge::{Bridge, HeaderFlag, HeaderState};

/// 标题栏高度（PLAN-NATIVE-UI.md 布局：row 0 = 48px）。
pub const HEADER_HEIGHT: f64 = 48.0;

/// 图标按钮（subtle；active = accent 高亮）。
///
/// reactor 未封装 ToolTipService → icon-only，与侧栏 `icon_button` 视觉
/// 语言一致（偏差 D6，WORKFLOW §3）。
fn action_button(
    icon: Icon,
    label: &'static str,
    automation_id: &'static str,
    enabled: bool,
    active: bool,
    on_click: impl Fn() + 'static,
) -> Element {
    let mut btn = button("")
        .icon(icon)
        .subtle()
        .enabled(enabled)
        .tooltip(label)
        .automation_name(label)
        .automation_id(automation_id)
        .on_click(on_click);
    if active {
        btn = btn.accent();
    }
    btn.into()
}

/// XAML 标题栏组件（放入外层 Grid row 0；host 检测到 TitleBar 自动接线）。
pub fn header(cx: &mut RenderCx, bridge: Arc<Bridge>) -> Element {
    let (state, set_state) = cx.use_state::<HeaderState>(HeaderState::default());
    let (lang, set_lang) = cx.use_state::<String>("zh".to_string());
    let timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    let lang_timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    let last_rev = cx.use_ref::<u64>(0);
    let last_lang_rev = cx.use_ref::<u64>(0);

    // 首次挂载：500ms rev 轮询（同 sidebar 模式；shell::poll_rev helper）。
    cx.use_effect((), {
        let bridge = bridge.clone();
        let set_state = set_state.clone();
        let timer = timer.clone();
        let last_rev = last_rev.clone();
        move || {
            crate::shell::poll_rev(
                timer,
                last_rev,
                Duration::from_millis(500),
                move || bridge.core().header_snapshot(),
                move |s| set_state.call(s),
            );
        }
    });

    // Locale is owned by config rather than HeaderState. Poll its independent
    // revision so changing language updates tooltips without requiring an
    // unrelated conversation/header event.
    cx.use_effect((), {
        let bridge = bridge.clone();
        let set_lang = set_lang.clone();
        let lang_timer = lang_timer.clone();
        let last_lang_rev = last_lang_rev.clone();
        move || {
            crate::shell::poll_rev(
                lang_timer,
                last_lang_rev,
                Duration::from_millis(500),
                move || bridge.core().settings_snapshot(),
                move |s| set_lang.call(s.map(|v| v.lang).unwrap_or_else(|| "zh".into())),
            );
        }
    });

    // 系统主题：reactor 由 ActualThemeChanged 驱动更新（engine.rs:733），
    // WebView 移除后无需回传 Web——轮询与 emit_theme_changed 一并删除。

    // ── 点击分发（①②③ 壳直接；④-⑦ 直连动作，协议请求 Rust 直发）──
    let on_workspace = {
        let bridge = bridge.clone();
        move || match bridge.pick_workspace_directory() {
            // 取消 → Ok(null) → 不动作；选择 → 直发 workspace.set（不再回传 Web）。
            Ok(serde_json::Value::String(path)) => {
                bridge.spawn_workspace_set(path);
            }
            _ => {}
        }
    };
    let on_location = {
        let bridge = bridge.clone();
        let workspace = state.workspace.clone();
        move || {
            if !workspace.is_empty() {
                let _ = bridge.open_path(&workspace);
            }
        }
    };
    let on_info = {
        let bridge = bridge.clone();
        move || bridge.toggle_header_flag(HeaderFlag::Info)
    };
    let on_stats = {
        let bridge = bridge.clone();
        move || bridge.toggle_header_flag(HeaderFlag::Stats)
    };
    let on_undo = {
        let bridge = bridge.clone();
        move || bridge.spawn_undo_last_turn()
    };
    let on_compact = {
        let bridge = bridge.clone();
        move || {
            bridge.spawn_conversation_command(
                deepx_client::ConversationCommand::ConversationCompact { turn_id: None },
            )
        }
    };

    // ── footer 槽：7 个 subtle 图标按钮（⑧pet 隐藏，规划决策）──
    let divider: Element = border(text_block(""))
        .width(1.0)
        .height(18.0)
        .background(ThemeRef::DividerStroke)
        .vertical_alignment(VerticalAlignment::Center)
        .into();
    let en = lang == "en";
    let workspace_label = if en {
        "Choose workspace"
    } else {
        "选择工作区"
    };
    let location_label = if en {
        "Open workspace in File Explorer"
    } else {
        "在文件资源管理器中打开工作区"
    };
    let info_label = if en {
        "Session information"
    } else {
        "会话信息"
    };
    let stats_label = if en {
        "Usage statistics"
    } else {
        "用量统计"
    };
    let undo_label = if en {
        "Undo last turn"
    } else {
        "撤销上一轮"
    };
    let compact_label = if state.compacting {
        if en {
            "Compacting context…"
        } else {
            "正在压缩上下文…"
        }
    } else if en {
        "Compact context"
    } else {
        "压缩上下文"
    };
    let compact_progress: Element = if state.compacting {
        ProgressRing::default()
            .width(16.0)
            .height(16.0)
            .automation_name(compact_label)
            .into()
    } else {
        grid(()).into()
    };
    let footer: Element = hstack((
        // 图标映射（bindings Symbol 枚举裁剪版，WORKFLOW §6.1 记录）：
        // ①OpenLocal ②OpenFile ┃ ③ContactInfo(替代 Info) ④FourBars(替代 Diagnostic)
        // ⑤Undo ⑥Clear(替代 Compress)（⑧pet 隐藏；console 随 WebView 移除）。
        action_button(
            Icon::symbol(Symbol::OpenLocal),
            workspace_label,
            "header-workspace",
            true,
            false,
            on_workspace,
        ),
        action_button(
            Icon::symbol(Symbol::OpenFile),
            location_label,
            "header-location",
            true,
            false,
            on_location,
        ),
        divider,
        action_button(
            Icon::symbol(Symbol::ContactInfo),
            info_label,
            "header-info",
            true,
            state.info_open,
            on_info,
        ),
        action_button(
            Icon::symbol(Symbol::FourBars),
            stats_label,
            "header-stats",
            true,
            state.stats_open,
            on_stats,
        ),
        action_button(
            Icon::symbol(Symbol::Undo),
            undo_label,
            "header-undo",
            !state.undo_disabled,
            false,
            on_undo,
        ),
        compact_progress,
        action_button(
            Icon::symbol(Symbol::Clear),
            compact_label,
            "header-compact",
            !(state.compacting || state.compact_disabled),
            false,
            on_compact,
        ),
    ))
    .spacing(4.0)
    .vertical_alignment(VerticalAlignment::Center)
    .into();

    // ── TitleBar：title 槽 = 会话标题 / 视图名 ─────────────────
    TitleBar::new(&state.title)
        .footer(footer)
        .tall(false)
        .into()
}
