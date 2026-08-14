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

/// 标题栏动作按钮：icon 上方按钮 + label 默认显示在 icon 之下。
///
/// WinUI Button 的 Icon+Content 只能水平排列，无法「icon 之下带 label」；
/// 自组 vstack(icon 按钮, label)：icon 区域走 Button（hover/按压/active
/// 高亮原生语义），label 区域同样可点（外层 on_tapped，Button 内部已
/// handled 不冒泡，无双触发）。label 常显——裸图标按钮无中文说明，
/// 与 Windows 右键菜单缺 label 同样令人困惑（D6 撤销）。
fn action_button(
    icon: Icon,
    label: &'static str,
    automation_id: &'static str,
    enabled: bool,
    active: bool,
    on_click: impl Fn() + Clone + 'static,
) -> Element {
    let mut btn = button("")
        .icon(icon)
        .subtle()
        .enabled(enabled)
        .tooltip(label)
        .automation_name(label)
        .automation_id(automation_id)
        .on_click(on_click.clone());
    if active {
        btn = btn.accent();
    }
    let btn_el: Element = btn.into();
    vstack((
        btn_el,
        text_block(label)
            .font_size(10.0)
            .foreground(if enabled {
                ThemeRef::SecondaryText
            } else {
                ThemeRef::DisabledText
            }),
    ))
    .spacing(1.0)
    .on_tapped(move || {
        if enabled {
            on_click();
        }
    })
    .automation_name(label)
    .automation_id(automation_id)
    .tooltip(label)
    .into()
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
                "header",
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
                "header_lang",
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

    // ── 点击分发（①②壳直接；③-⑥ 直连动作，协议请求 Rust 直发）──
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
    let on_stats = {
        let bridge = bridge.clone();
        move || bridge.toggle_header_flag(HeaderFlag::Stats)
    };
    let on_compact = {
        let bridge = bridge.clone();
        move || {
            bridge.spawn_conversation_command(
                deepx_client::ConversationCommand::ConversationCompact { turn_id: None },
            )
        }
    };

    // ── 刷新（安慰剂 + 真实重拉：会话列表 + 配置 force）────────────
    // 点击 → 按钮转圈 900ms（"正在刷新"仪式感）→ 自动复位。数据重拉是
    // 异步的，各视图 500ms 轮询自动反映结果——刷新本身无需等待。
    let (refreshing, set_refreshing) = cx.use_state::<bool>(false);
    let refresh_timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    let on_refresh = {
        let bridge = bridge.clone();
        let set_refreshing = set_refreshing.clone();
        let refresh_timer = refresh_timer.clone();
        move || {
            bridge.spawn_refresh_sessions();
            bridge.spawn_config_load(true);
            set_refreshing.call(true);
            if refresh_timer.borrow().is_none() {
                if let Ok(t) = DispatcherTimer::new(Duration::from_millis(900), {
                    let set_refreshing = set_refreshing.clone();
                    let refresh_timer = refresh_timer.clone();
                    move || {
                        set_refreshing.call(false);
                        if let Some(t) = refresh_timer.borrow().as_ref() {
                            let _ = t.stop();
                        }
                        *refresh_timer.borrow_mut() = None;
                    }
                }) {
                    *refresh_timer.borrow_mut() = Some(t);
                }
            }
        }
    };

    // ── footer 槽：工作区（按钮+路径显示）+ 用量统计 + 压缩 + 刷新 ──
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
    // 工作区路径只读显示（替代原「在资源管理器中打开」按钮）：当前选择
    // 一目了然；过长省略号裁剪（标题栏空间有限），完整路径进 tooltip 语义
    // 由 automation_name 承载（Element 无 tooltip 能力）。
    let workspace_path: Element = if state.workspace.is_empty() {
        text_block(if en { "No workspace" } else { "未选择工作区" })
            .font_size(11.0)
            .foreground(ThemeRef::TertiaryText)
            .vertical_alignment(VerticalAlignment::Center)
            .automation_name("工作区路径（空）")
            .with_key("header-workspace-path-empty")
            .into()
    } else {
        text_block(&state.workspace)
            .font_size(11.0)
            .foreground(ThemeRef::SecondaryText)
            .max_width(260.0)
            .text_trimming(TextTrimming::CharacterEllipsis)
            .vertical_alignment(VerticalAlignment::Center)
            .automation_name(&state.workspace)
            .with_key("header-workspace-path")
            .into()
    };
    let stats_label = if en {
        "Usage statistics"
    } else {
        "用量统计"
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
    let refresh_label = if refreshing {
        if en {
            "Refreshing…"
        } else {
            "正在刷新…"
        }
    } else if en {
        "Refresh"
    } else {
        "刷新"
    };
    // 转圈期间图标按钮替换为 ProgressRing（同 compact 模式），禁用防连点。
    let refresh_el: Element = if refreshing {
        ProgressRing::default()
            .width(16.0)
            .height(16.0)
            .automation_name(refresh_label)
            .into()
    } else {
        action_button(
            Icon::symbol(Symbol::Refresh),
            refresh_label,
            "header-refresh",
            true,
            false,
            on_refresh,
        )
    };
    let footer: Element = hstack((
        action_button(
            Icon::symbol(Symbol::OpenLocal),
            workspace_label,
            "header-workspace",
            true,
            false,
            on_workspace,
        ),
        workspace_path,
        divider.clone(),
        action_button(
            Icon::symbol(Symbol::FourBars),
            stats_label,
            "header-stats",
            true,
            state.stats_open,
            on_stats,
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
        divider.clone(),
        refresh_el,
    ))
    .spacing(6.0)
    .vertical_alignment(VerticalAlignment::Center)
    .into();

    // ── TitleBar：title 槽 = 会话标题 / 视图名 ─────────────────
    TitleBar::new(&state.title)
        .footer(footer)
        .tall(false)
        .into()
}
